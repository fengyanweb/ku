use super::*;
use crate::runtime::task::{current_execution_termination, TaskHandle, TaskRuntimeSnapshot};
use crate::runtime::value::BorrowedValue;
use std::cell::Cell;
use std::sync::{atomic::AtomicBool, mpsc, Weak};

const WAIT: Duration = Duration::from_secs(3);
const NESTED: &str = r#"
fn Echo(n: int): int { return n + 1 }
fn Target(a: int, b: int): int { return a * 100 + b }
fn Entry(n: int): int {
    marker = 7
    return Target(Echo(n), Echo(n + 1))
}
"#;
const SIMPLE: &str = r#"
fn Target(a: int, b: int): int { return a + b }
fn Entry(n: int): int { marker = 7
    return Target(n, 1) }
"#;
const ERROR: &str = r#"
fn First(): int { return 1 / 0 }
fn Later(): int { return 9223372036854775807 + 1 }
fn Target(a: int, b: int): int { return a + b }
fn Entry(n: int): int { marker = 7
    return Target(First(), Later()) }
"#;
const ASYNC: &str = r#"
async fn Child(n: int): int! { return ok(n + 1) }
async fn Parent(n: int): int! {
    marker = 7
    task = Child(n)
    return ok((await task)? + 1)
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
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        thread::yield_now();
    }
    predicate()
}
struct Blocker {
    release: Gate,
    _owner: TaskHandle,
}
impl Blocker {
    fn start(runtime: &TaskRuntime) -> Self {
        let (release, released) = Gate::pair();
        let (tx, rx) = mpsc::sync_channel(1);
        let owner = runtime
            .spawn(move || {
                tx.send(thread::current().id()).unwrap();
                released
                    .recv_timeout(WAIT)
                    .expect("finite actual worker gate");
                Ok(Value::Null)
            })
            .unwrap();
        assert_ne!(rx.recv_timeout(WAIT).unwrap(), thread::current().id());
        Self {
            release,
            _owner: owner,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outgoing {
    Fail,
    Task(i64, &'static str),
    Other,
    Error,
}
fn outgoing(result: &KuResult<Flow>) -> Outgoing {
    match result {
        Ok(Flow::Fail { .. }) => Outgoing::Fail,
        Ok(Flow::Return {
            value: Value::Task(task),
            ..
        }) => Outgoing::Task(task.id(), task.status()),
        Ok(_) => Outgoing::Other,
        Err(_) => Outgoing::Error,
    }
}
struct ArgumentHooks {
    trace: TraceHooks,
    target: Option<Span>,
    inject: Option<TaskHandle>,
    prepared: Vec<(Span, usize, usize)>,
    done: Vec<(Span, Outgoing, usize)>,
    fallback: Vec<(Span, usize, usize, i64, bool)>,
}
impl ArgumentHooks {
    fn new(source: &HashMap<String, FnDecl>, runtime: &TaskRuntime) -> Self {
        let mut trace = TraceHooks::new();
        result_entry::install(&mut trace.host, source);
        trace.host.task_runtime = Some(runtime.clone());
        Self {
            trace,
            target: None,
            inject: None,
            prepared: Vec::new(),
            done: Vec::new(),
            fallback: Vec::new(),
        }
    }
    fn witness(
        &mut self,
        env: &Env,
        leases: usize,
        values: usize,
        result: &KuResult<Flow>,
        span: Span,
    ) {
        if self.target == Some(span) {
            self.fallback.push((
                span,
                leases,
                values,
                scalar(env, "marker"),
                matches!(
                    result,
                    Ok(Flow::Return {
                        value: Value::Int(3),
                        ..
                    })
                ),
            ));
        }
    }
}
impl ExecutionHooks for ArgumentHooks {
    fn host(&mut self) -> &mut Interpreter {
        &mut self.trace.host
    }
    fn tick(&mut self, span: Span) -> KuResult<()> {
        self.trace.tick(span)
    }
    fn prepare_arguments_exit(
        &mut self,
        env: &mut Env,
        leases: &[BorrowLease],
        values: &mut [Value],
        span: Span,
    ) -> KuResult<()> {
        if self.target == Some(span) {
            if let Some(task) = self.inject.take() {
                // A real sole owner replaces one already reserved scalar slot
                // after the actual callee completed. No source Owned parameter
                // admission, argument evaluation, or Vec growth is manufactured.
                let first = values.first_mut().expect("actual reserved argument slot");
                drop(std::mem::replace(first, Value::Task(task)));
            }
        }
        self.prepared.push((span, leases.len(), values.len()));
        self.trace.prepare_arguments_exit(env, leases, values, span)
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
    fn finish_call_with_feedback(
        &mut self,
        env: Env,
        leases: Vec<BorrowLease>,
        result: KuResult<Flow>,
        span: Span,
        kind: CallKind,
        feedback: &Cell<Option<RuntimeTermination>>,
    ) -> KuResult<Flow> {
        self.trace
            .finish_call_with_feedback(env, leases, result, span, kind, feedback)
    }
    fn finish_call_value(
        &mut self,
        result: KuResult<Flow>,
        span: Span,
        kind: CallKind,
    ) -> KuResult<Flow> {
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
        self.witness(env, leases.len(), values.len(), &result, span);
        self.trace
            .finish_arguments(env, leases, values, result, span)
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
        self.witness(env, leases.len(), values.len(), &result, span);
        self.trace
            .finish_arguments_with_feedback(env, leases, values, result, span, feedback)
    }
    fn finish_arguments_value(&mut self, result: KuResult<Flow>, span: Span) -> KuResult<Flow> {
        self.done
            .push((span, outgoing(&result), self.trace.host.caller_owners.len()));
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
fn return_call(source: &HashMap<String, FnDecl>) -> &Expr {
    let Stmt::Return {
        value: Some(value), ..
    } = &source["Entry"].body[1]
    else {
        unreachable!()
    };
    assert!(matches!(value.kind, ExprKind::Call { .. }));
    value
}
fn async_call_span(source: &HashMap<String, FnDecl>) -> Span {
    match &source["Parent"].body[1] {
        Stmt::Assign { value, .. } | Stmt::VarDecl { value, .. } => value.span,
        _ => unreachable!(),
    }
}
fn new_machine(source: &HashMap<String, FnDecl>, claimed: bool) -> Machine {
    let name = if claimed { "Parent" } else { "Entry" };
    let code = if claimed {
        CodeHandle::compile_claimed_async_entry(source, &source[name], Limits::default()).unwrap()
    } else {
        CodeHandle::compile_named(source, &source[name], Limits::default()).unwrap()
    };
    let mut machine = Machine::from_code(code, Limits::default(), 0);
    machine
        .start_once(name, &[Value::Int(2)], 0, false)
        .unwrap();
    machine
}
fn step(machine: &mut Machine, hooks: &mut ArgumentHooks) -> ChunkProgress {
    let _execution = ExecutionTerminationGuard::enter();
    machine
        .dispatch_chunk(NonZeroUsize::new(1).unwrap(), hooks)
        .unwrap()
}
fn to_marker(machine: &mut Machine, hooks: &mut ArgumentHooks, span: Span, prepare: bool) {
    for _ in 0..20_000 {
        let top = machine
            .work
            .len()
            .checked_sub(1)
            .and_then(|index| machine.work.get(index));
        let found = if prepare {
            matches!(top, Some(Work::PrepareArguments { span: at, .. }) if machine.code.span(*at) == span)
        } else {
            matches!(top, Some(Work::ArgumentsExit(frame)) if machine.code.span(frame.span) == span)
        };
        if found && machine.exits.len() == 0 {
            return;
        }
        assert!(!matches!(
            step(machine, hooks),
            ChunkProgress::Finished { .. }
        ));
    }
    panic!("finite source did not reach its actual argument marker");
}
fn finish(machine: &mut Machine, hooks: &mut ArgumentHooks) -> KuResult<Flow> {
    for _ in 0..20_000 {
        match step(machine, hooks) {
            ChunkProgress::More { .. } => {}
            ChunkProgress::Waiting { .. } => machine.wait_synchronously(&hooks.trace.host),
            ChunkProgress::Finished { result, .. } => return result,
        }
    }
    panic!("finite source did not complete arguments");
}
fn retire(runtime: &TaskRuntime, count: usize) {
    assert!(until(|| runtime.snapshot().unwrap().finished_tasks == count));
}
fn drain(runtime: &TaskRuntime, count: usize) {
    retire(runtime, count);
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
        (0, 0, 0, 0, count)
    );
}
fn healthy_and_drain(runtime: &TaskRuntime, previous: usize) {
    retire(runtime, previous);
    let (tx, rx) = mpsc::sync_channel(1);
    let task = runtime
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
    let id = task.id();
    let receipt = rx.recv_timeout(WAIT);
    retire(runtime, previous + 1);
    let result = task.await_result();
    drop(task);
    drain(runtime, previous + 1);
    assert_eq!(receipt.unwrap(), (None, None, id));
    assert_eq!(result.unwrap(), Value::Int(7));
}

#[test]
fn nested_scalar_arguments_keep_real_caller_observers_and_buffers_until_done() {
    let runtime = TaskRuntime::with_limits(1, 1024, 0, 0, 1024);
    let activation = runtime.spawn(|| Ok(Value::Null)).unwrap();
    retire(&runtime, 1);
    activation.await_result().unwrap();
    drop(activation);
    let source = functions(NESTED);
    let mut machine = new_machine(&source, false);
    let mut hooks = ArgumentHooks::new(&source, &runtime);
    let mut phases = Vec::new();
    let mut result = None;
    for _ in 0..20_000 {
        let active = machine.exits.get(0).and_then(|exit| {
            let ExitCompletion::Arguments {
                frame,
                staged_capacity,
            } = &exit.completion
            else {
                return None;
            };
            let phase = exit.state.phase();
            let span = machine.code.span(frame.span);
            assert_eq!(exit.owner_call, 1);
            assert_eq!((machine.calls.len(), machine.call_depth), (1, 1));
            assert_eq!(scalar(&machine.calls.get(0).unwrap().env, "n"), 2);
            assert_eq!(scalar(&machine.calls.get(0).unwrap().env, "marker"), 7);
            assert_eq!(
                hooks.trace.host.caller_owners.len(),
                frame.previous_observers + 1
            );
            assert_eq!(
                machine.argument_bytes,
                machine.argument_capacity * std::mem::size_of::<Value>()
            );
            assert!(machine.argument_capacity >= *staged_capacity);
            assert!(machine.pending.is_none());
            assert_eq!(exit.state.context(), None);
            if matches!(
                phase,
                cancellation::ExitPhase::DeadlineCheck
                    | cancellation::ExitPhase::Arbitrate
                    | cancellation::ExitPhase::Done
            ) {
                assert!(frame.staged.is_empty() && frame.leases.is_empty());
                assert_eq!(*staged_capacity, 0);
            } else {
                assert_eq!(frame.staged.capacity(), *staged_capacity);
                assert!(frame.staged.len() == 1 || frame.staged.len() == 2);
            }
            phases.push((span, phase));
            Some((
                phase,
                frame.previous_observers,
                machine.work.len(),
                machine.argument_capacity,
                *staged_capacity,
                hooks.trace.ticks.len(),
            ))
        });
        let progress = step(&mut machine, &mut hooks);
        if let Some((phase, previous, work, capacity, staged, ticks)) = active {
            assert_eq!(machine.work.len(), work);
            assert_eq!((machine.calls.len(), machine.call_depth), (1, 1));
            assert_eq!(hooks.trace.ticks.len(), ticks);
            let expected_capacity = if phase == cancellation::ExitPhase::Teardown {
                capacity - staged
            } else {
                capacity
            };
            assert_eq!(machine.argument_capacity, expected_capacity);
            assert_eq!(
                hooks.trace.host.caller_owners.len(),
                if phase == cancellation::ExitPhase::Done {
                    previous
                } else {
                    previous + 1
                }
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
    let outer = return_call(&source);
    let ExprKind::Call { args, .. } = &outer.kind else {
        unreachable!()
    };
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
    for span in [args[0].span, args[1].span, outer.span] {
        assert_eq!(
            phases
                .iter()
                .filter_map(|(at, phase)| (*at == span).then_some(*phase))
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
        Value::Int(304)
    );
    assert_eq!(
        (
            machine.metrics.entered_arguments_exits,
            machine.metrics.retired_arguments_exits
        ),
        (3, 3)
    );
    assert_eq!(
        (machine.metrics.entered_calls, machine.metrics.retired_calls),
        (4, 4)
    );
    assert_eq!((machine.argument_capacity, machine.argument_bytes), (0, 0));
    assert!(hooks.trace.host.caller_owners.is_empty());
    assert_eq!(hooks.done.len(), 3);
    assert!(hooks.done.iter().all(|(_, _, observers)| *observers > 0));
    assert_drained(&machine.metrics, 0);
    eprintln!(
        "arguments nested phases={phases:?} header={} argument_peak={} retained={}",
        std::mem::size_of::<PendingScopeExit>(),
        machine.peak_argument_bytes,
        machine.exits.charged_storage_bytes()
    );
    drop(machine);
    drain(&runtime, 1);
}

#[test]
fn first_argument_error_skips_later_source_and_sole_argument_fail_stays_flow() {
    for fail in [false, true] {
        let runtime = TaskRuntime::with_limits(1, 1024, 0, 0, 1024);
        let source = functions(ERROR);
        let span = return_call(&source).span;
        let mut machine = new_machine(&source, false);
        let mut hooks = ArgumentHooks::new(&source, &runtime);
        to_marker(&mut machine, &mut hooks, span, false);
        assert_eq!(
            machine
                .pending
                .as_ref()
                .unwrap()
                .as_ref()
                .err()
                .expect("real first-argument error")
                .message,
            "division by zero"
        );
        if fail {
            let actual = functions("fn Failure(): int! { fail \"sentinel\" }");
            let flow = Interpreter::new()
                .exec_stmt(&actual["Failure"].body[0], &mut Env::new(), 0)
                .unwrap();
            assert!(matches!(flow, Flow::Fail { .. }));
            // Move a real evaluator-produced sole Flow at this kernel boundary;
            // no Fail syntax is claimed to enter the scalar source compiler.
            drop(machine.pending.replace(Ok(flow)));
        }
        for _ in 0..32 {
            assert!(!matches!(
                step(&mut machine, &mut hooks),
                ChunkProgress::Finished { .. }
            ));
            if hooks.done.iter().any(|(at, _, _)| *at == span) {
                break;
            }
        }
        let at_done = hooks.done.iter().find(|(at, _, _)| *at == span).unwrap().1;
        if fail {
            assert_eq!(at_done, Outgoing::Fail);
            assert!(matches!(
                machine.pending.as_ref(),
                Some(Ok(Flow::Fail { .. }))
            ));
        } else {
            assert_eq!(at_done, Outgoing::Error);
        }
        let result = finish(&mut machine, &mut hooks);
        assert_eq!(
            hooks
                .prepared
                .iter()
                .filter(|(at, _, _)| *at == span)
                .count(),
            1
        );
        assert_eq!(
            hooks.done.iter().filter(|(at, _, _)| *at == span).count(),
            1
        );
        assert!(!hooks
            .trace
            .ticks
            .contains(&stmt_span(&source["Later"].body[0])));
        assert!(!hooks
            .trace
            .ticks
            .contains(&stmt_span(&source["Target"].body[0])));
        assert_eq!(
            (machine.metrics.entered_calls, machine.metrics.retired_calls),
            (2, 2)
        );
        assert_eq!((machine.argument_capacity, machine.argument_bytes), (0, 0));
        assert_drained(&machine.metrics, 0);
        if fail {
            let value = result
                .unwrap()
                .into_call_value(Span::default(), "fixture")
                .unwrap();
            let Value::Result { ok: false, value } = value else {
                panic!("root call must normalize Fail once");
            };
            let Value::Object(error) = *value else {
                panic!("no nested Result wrapping");
            };
            assert_eq!(
                error.get("message"),
                Some(&Value::String("sentinel".into()))
            );
        } else {
            assert_eq!(
                result
                    .err()
                    .expect("first-argument failure remains failure")
                    .message,
                "division by zero"
            );
        }
        drop(machine);
        drain(&runtime, 0);
    }
}

#[test]
fn actual_async_return_task_is_sole_primary_until_arguments_done_and_emitted_once() {
    let runtime = TaskRuntime::with_limits(1, 1024, 0, 0, 1024);
    let mut blocked = Blocker::start(&runtime);
    let source = functions(ASYNC);
    let span = async_call_span(&source);
    let mut machine = new_machine(&source, true);
    let mut hooks = ArgumentHooks::new(&source, &runtime);
    hooks.trace.host.async_execution = true;
    to_marker(&mut machine, &mut hooks, span, false);
    let Some(Ok(Flow::Return {
        value: Value::Task(task),
        ..
    })) = machine.pending.as_ref()
    else {
        panic!("production async submission retains its sole Task");
    };
    let id = task.id();
    assert_eq!(task.status(), "pending");
    let marker_index = machine
        .work
        .len()
        .checked_sub(1)
        .expect("actual async arguments marker");
    let Work::ArgumentsExit(frame) = machine.work.get(marker_index).unwrap() else {
        unreachable!()
    };
    assert_eq!(
        (
            frame.staged.len(),
            frame.staged.capacity(),
            machine.argument_capacity
        ),
        (0, 0, 0)
    );
    for _ in 0..32 {
        assert!(!matches!(
            step(&mut machine, &mut hooks),
            ChunkProgress::Finished { .. }
        ));
        let snapshot = runtime.snapshot().unwrap();
        assert_eq!(
            (
                snapshot.active_tasks,
                snapshot.queued_tasks,
                snapshot.finished_tasks,
                snapshot.cancelled_tasks
            ),
            (2, 1, 0, 0)
        );
        assert_eq!(
            (current_cleanup_context(), current_execution_termination()),
            (None, None)
        );
        if hooks.done.iter().any(|(at, _, _)| *at == span) {
            break;
        }
        let exit = machine.exits.get(0).unwrap();
        assert!(matches!(&exit.completion, ExitCompletion::Arguments { .. }));
        assert_eq!(exit.state.context(), None);
        assert_eq!(
            machine.exit_handle_bytes, 0,
            "sole outgoing Task is not a cleanup argument"
        );
    }
    assert_eq!(
        hooks.done.iter().filter(|(at, _, _)| *at == span).count(),
        1
    );
    assert_eq!(
        hooks.done.iter().find(|(at, _, _)| *at == span).unwrap().1,
        Outgoing::Task(id, "pending")
    );
    assert!(machine.pending.is_none());
    assert!(!machine.calls.get(0).unwrap().env.contains("task"));
    assert_eq!(
        (0..machine.values.len())
            .filter(
                |at| matches!(machine.values.get(*at), Some(Value::Task(task)) if task.id() == id)
            )
            .count(),
        1
    );
    blocked.release.release();
    // Observe genuine worker retirement before host-side Await can help.
    retire(&runtime, 2);
    let result = finish(&mut machine, &mut hooks)
        .unwrap()
        .into_call_value(Span::default(), "fixture")
        .unwrap();
    let snapshot = runtime.snapshot().unwrap();
    drop(machine);
    drain(&runtime, 2);
    assert_eq!(
        result,
        Value::Result {
            ok: true,
            value: Box::new(Value::Int(4))
        }
    );
    assert_eq!(
        (
            snapshot.accepted_submissions,
            snapshot.finished_tasks,
            snapshot.cancelled_tasks
        ),
        (2, 2, 0)
    );
}

#[derive(Debug)]
struct DropReceipt {
    context: Option<RuntimeTermination>,
    leased_root_alive: bool,
    parent_alive: bool,
    snapshot: TaskRuntimeSnapshot,
}
struct Capture {
    lease: Option<Arc<HttpListenerLease>>,
    shorter: Option<RuntimeTermination>,
    leased: Option<BorrowedValue>,
    parent: Option<Weak<HttpListenerLease>>,
    runtime: TaskRuntime,
    receipt: SyncSender<DropReceipt>,
    drops: Arc<AtomicUsize>,
    panic: bool,
}
impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(shorter) = self.shorter {
            drop(CleanupGuard::enter(shorter));
        }
        drop(self.lease.take());
        self.drops.fetch_add(1, Ordering::AcqRel);
        let _ = self.receipt.try_send(DropReceipt {
            context: current_cleanup_context(),
            leased_root_alive: self
                .leased
                .as_ref()
                .is_some_and(|view| view.with_read(Span::default(), |_| Ok(())).is_ok()),
            parent_alive: self
                .parent
                .as_ref()
                .is_some_and(|parent| parent.strong_count() != 0),
            snapshot: self.runtime.snapshot().unwrap(),
        });
        if self.panic {
            panic!("actual arguments capture native Drop panic");
        }
    }
}
fn listener() -> (Arc<HttpListenerLease>, Weak<HttpListenerLease>) {
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let lease = HttpListenerLease::new(http_listener_registry::insert(socket).unwrap());
    let observer = Arc::downgrade(&lease);
    (lease, observer)
}
fn queued_capture(
    runtime: &TaskRuntime,
    shorter: Option<RuntimeTermination>,
    leased: Option<BorrowedValue>,
    parent: Option<Weak<HttpListenerLease>>,
    panic: bool,
) -> (
    TaskHandle,
    Receiver<DropReceipt>,
    Weak<HttpListenerLease>,
    Arc<AtomicUsize>,
    Arc<AtomicBool>,
) {
    let (lease, observer) = listener();
    let (tx, rx) = mpsc::sync_channel(1);
    let drops = Arc::new(AtomicUsize::new(0));
    let ran = Arc::new(AtomicBool::new(false));
    let probe = Capture {
        lease: Some(lease),
        shorter,
        leased,
        parent,
        runtime: runtime.clone(),
        receipt: tx,
        drops: drops.clone(),
        panic,
    };
    let executed = ran.clone();
    let task = runtime
        .spawn(move || {
            executed.store(true, Ordering::Release);
            drop(probe);
            Ok(Value::Null)
        })
        .unwrap();
    (task, rx, observer, drops, ran)
}

#[test]
fn arguments_slot_handle_and_staged_budget_failures_cleanup_without_replaying_source() {
    let source = functions(SIMPLE);
    let span = return_call(&source).span;
    let runtime = TaskRuntime::with_limits(1, 1024, 0, 0, 1024);
    let mut machine = new_machine(&source, false);
    let mut hooks = ArgumentHooks::new(&source, &runtime);
    to_marker(&mut machine, &mut hooks, span, true);
    machine.value_limits.max_storage_bytes = machine.values.charged_storage_bytes();
    let result = finish(&mut machine, &mut hooks);
    assert!(result
        .err()
        .expect("staged backing admission must fail")
        .message
        .contains("internal execution storage rejected"));
    assert!(hooks.prepared.is_empty() && hooks.done.is_empty());
    assert!(!hooks
        .trace
        .ticks
        .contains(&stmt_span(&source["Target"].body[0])));
    assert_eq!(
        (machine.metrics.entered_calls, machine.metrics.retired_calls),
        (1, 1)
    );
    assert_eq!(
        (
            machine.argument_capacity,
            machine.argument_bytes,
            machine.peak_argument_bytes
        ),
        (0, 0, 0)
    );
    drop(machine);
    drain(&runtime, 0);

    for handles in [false, true] {
        let runtime = TaskRuntime::with_limits(1, 1024, 0, 0, 1024);
        let mut blocked = Blocker::start(&runtime);
        let (task, rx, observer, drops, ran) = queued_capture(&runtime, None, None, None, false);
        let mut machine = new_machine(&source, false);
        let mut hooks = ArgumentHooks::new(&source, &runtime);
        hooks.target = Some(span);
        hooks.inject = Some(task);
        to_marker(&mut machine, &mut hooks, span, false);
        assert!(matches!(
            machine.pending.as_ref(),
            Some(Ok(Flow::Return {
                value: Value::Int(3),
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
        let receipt = rx.try_recv();
        let before = runtime.snapshot().unwrap();
        let argument_metrics = (
            machine.metrics.entered_arguments_exits,
            machine.metrics.retired_arguments_exits,
        );
        let accounting = (
            machine.argument_capacity,
            machine.argument_bytes,
            machine.exit_handle_capacity,
            machine.exit_handle_bytes,
            machine.peak_exit_handle_bytes,
        );
        drop(machine);
        blocked.release.release();
        healthy_and_drain(&runtime, 2);
        eprintln!("arguments budget handles={handles} fallback={:?} receipt={receipt:?} before_retirement={before:?}", hooks.fallback);
        assert_eq!(hooks.fallback, vec![(span, 0, 2, 7, true)]);
        assert_eq!(
            hooks
                .prepared
                .iter()
                .filter(|(at, _, _)| *at == span)
                .count(),
            1
        );
        assert!(hooks.done.is_empty());
        assert_eq!(argument_metrics, (0, 0));
        assert_eq!(accounting, (0, 0, 0, 0, 0));
        assert_eq!(
            hooks
                .trace
                .ticks
                .iter()
                .filter(|at| **at == stmt_span(&source["Target"].body[0]))
                .count(),
            1
        );
        assert!(result
            .err()
            .expect("exit backing admission must fail")
            .message
            .contains("internal execution storage rejected"));
        assert_eq!(
            receipt.unwrap().context.unwrap().reason,
            TerminationReason::Cancelled
        );
        assert_eq!(observer.strong_count(), 0);
        assert_eq!(drops.load(Ordering::Acquire), 1);
        assert!(!ran.load(Ordering::Acquire));
        assert_eq!(
            (
                before.active_tasks,
                before.queued_tasks,
                before.finished_tasks
            ),
            (1, 0, 1)
        );
        assert_eq!(before.registered_tasks, 1);
    }
}

fn contexts() -> (RuntimeTermination, RuntimeTermination, RuntimeTermination) {
    let now = Instant::now();
    (
        RuntimeTermination {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: now + WAIT,
        },
        RuntimeTermination {
            reason: TerminationReason::TimedOut,
            cleanup_deadline: now + Duration::from_secs(2),
        },
        RuntimeTermination {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: now + Duration::from_secs(1),
        },
    )
}
fn observe_cancel(hooks: &mut ArgumentHooks, primary: RuntimeTermination, span: Span) {
    let _execution = ExecutionTerminationGuard::enter();
    let incoming: KuResult<Flow> = Err(KuError::termination(primary, span));
    hooks.trace.host.observe_termination(&incoming);
}

#[test]
fn actual_legacy_values_drop_precedes_restoring_leases_and_reports_native_minimum_d() {
    for panics in [false, true] {
        let runtime = TaskRuntime::with_limits(1, 1024, 0, 0, 1024);
        let mut blocked = Blocker::start(&runtime);
        let source = functions(SIMPLE);
        let span = return_call(&source).span;
        let mut machine = new_machine(&source, false);
        let mut hooks = ArgumentHooks::new(&source, &runtime);
        hooks.target = Some(span);
        to_marker(&mut machine, &mut hooks, span, false);
        let (primary, ambient, shorter) = contexts();
        let effective = RuntimeTermination {
            reason: ambient.reason,
            cleanup_deadline: shorter.cleanup_deadline,
        };
        let (leased, leased_observer) = listener();
        let env = &mut machine.calls.get_mut(0).unwrap().env;
        env.define_owned(
            "lease_owner".into(),
            Value::HttpListenerLease(leased),
            false,
            span,
        )
        .unwrap();
        let (view, lease) = env.borrow("lease_owner", span).unwrap();
        let Value::Borrowed(view) = view else {
            unreachable!()
        };
        let (task, rx, observer, drops, ran) =
            queued_capture(&runtime, Some(shorter), Some(view), None, panics);
        let mut captures = Env::new();
        captures
            .define_owned("child".into(), Value::Task(task), false, span)
            .unwrap();
        let value = Value::Function {
            params: Vec::new(),
            param_modes: Vec::new(),
            body: Vec::new(),
            captures,
            self_name: None,
            is_async: false,
        };
        let marker_index = machine
            .work
            .len()
            .checked_sub(1)
            .expect("actual arguments marker");
        let Work::ArgumentsExit(frame) = machine.work.get_mut(marker_index).unwrap() else {
            unreachable!()
        };
        // Explicit general kernel cleanup fixture: the original collector does
        // not traverse function captures. Its real queued Task owner therefore
        // drops during values Teardown while the restoring lease is still live.
        // Neither function argument nor borrowed mode is new source admission.
        assert_eq!(frame.staged.len(), 2);
        drop(std::mem::replace(&mut frame.staged[0], value));
        frame.leases.push(lease.unwrap());
        observe_cancel(&mut hooks, primary, span);
        let ambient_guard = CleanupGuard::enter(ambient);
        let observed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            step(&mut machine, &mut hooks)
        }));
        let saved = machine.fallback_release.get();
        let meta = hooks.trace.host.termination;
        let aborted = machine.aborted_release;
        let receipt = rx.try_recv();
        let restored = machine
            .calls
            .get(0)
            .unwrap()
            .env
            .with_value("lease_owner", span, |value| {
                Ok(matches!(value, Value::HttpListenerLease(_)))
            });
        let leased_before_parent = leased_observer.strong_count();
        let after = current_cleanup_context();
        drop(ambient_guard);
        let outside = (current_cleanup_context(), current_execution_termination());
        let result = if observed.is_ok() {
            Some(finish(&mut machine, &mut hooks))
        } else {
            None
        };
        let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(machine)));
        blocked.release.release();
        healthy_and_drain(&runtime, 2);
        eprintln!("arguments legacy native={panics} meta={meta:?} saved={saved:?} aborted={aborted:?} receipt={receipt:?}");
        let receipt = receipt.unwrap();
        assert!(
            receipt.leased_root_alive,
            "values native Drop must precede real lease restoration"
        );
        assert_eq!(receipt.context, Some(effective));
        assert!(restored.unwrap());
        assert_eq!(leased_before_parent, 1);
        assert_eq!(leased_observer.strong_count(), 0);
        assert_eq!(observer.strong_count(), 0);
        assert_eq!(drops.load(Ordering::Acquire), 1);
        assert!(!ran.load(Ordering::Acquire));
        assert_eq!(saved, Some(effective));
        assert_eq!(meta.unwrap().reason, primary.reason);
        assert_eq!(after, Some(effective));
        assert_eq!(outside, (None, None));
        assert!(dropped.is_ok());
        assert_eq!(hooks.fallback, vec![(span, 1, 2, 7, true)]);
        if panics {
            let panic = match observed {
                Err(panic) => panic,
                Ok(_) => panic!("real value destructor must panic"),
            };
            assert_eq!(
                panic.downcast_ref::<&str>().copied(),
                Some("actual arguments capture native Drop panic")
            );
            assert_eq!(aborted, Some(effective));
        } else {
            assert!(observed.is_ok());
            let flow = result.unwrap().unwrap();
            let Flow::Return { value, cleanup } = flow else {
                panic!("successful real collection preserves its sole return");
            };
            assert_eq!(value, Value::Int(3));
            assert_eq!(
                cleanup,
                Some(RuntimeTermination {
                    reason: primary.reason,
                    cleanup_deadline: shorter.cleanup_deadline,
                })
            );
            assert_eq!(aborted, None);
        }
    }
}

#[test]
fn abandoned_arguments_header_releases_staged_before_async_primary_and_parent_prefix() {
    for panics in [false, true] {
        let runtime = TaskRuntime::with_limits(1, 1024, 0, 0, 1024);
        let mut blocked = Blocker::start(&runtime);
        let source = functions(ASYNC);
        let span = async_call_span(&source);
        let mut machine = new_machine(&source, true);
        let mut hooks = ArgumentHooks::new(&source, &runtime);
        hooks.trace.host.async_execution = true;
        to_marker(&mut machine, &mut hooks, span, false);
        assert!(
            matches!(machine.pending.as_ref(), Some(Ok(Flow::Return { value: Value::Task(task), .. })) if task.status() == "pending")
        );
        let marker_index = machine
            .work
            .len()
            .checked_sub(1)
            .expect("actual async arguments marker");
        let Work::ArgumentsExit(frame) = machine.work.get(marker_index).unwrap() else {
            unreachable!()
        };
        assert_eq!((frame.staged.len(), frame.staged.capacity()), (0, 0));
        let (primary, ambient, shorter) = contexts();
        let effective = RuntimeTermination {
            reason: ambient.reason,
            cleanup_deadline: shorter.cleanup_deadline,
        };
        let (parent, parent_observer) = listener();
        machine
            .calls
            .get_mut(0)
            .unwrap()
            .env
            .define_owned(
                "parent_resource".into(),
                Value::HttpListenerLease(parent),
                false,
                span,
            )
            .unwrap();
        let (task, rx, observer, drops, ran) = queued_capture(
            &runtime,
            Some(shorter),
            None,
            Some(parent_observer.clone()),
            panics,
        );
        // Explicit kernel exit owner: use the actual production reserve/charge
        // before replacing the async marker's observed empty backing. The source
        // async call already moved its own scalar arguments to its child factory.
        let mut staged = machine.argument_storage(1, span).unwrap();
        let capacity = staged.capacity();
        staged.push(Value::Task(task));
        let marker_index = machine
            .work
            .len()
            .checked_sub(1)
            .expect("actual async arguments marker");
        let Work::ArgumentsExit(frame) = machine.work.get_mut(marker_index).unwrap() else {
            unreachable!()
        };
        assert!(frame.staged.is_empty() && frame.staged.capacity() == 0);
        frame.staged = staged;
        assert_eq!(
            machine.argument_bytes,
            capacity * std::mem::size_of::<Value>()
        );
        observe_cancel(&mut hooks, primary, span);
        let ambient_guard = CleanupGuard::enter(ambient);
        assert!(matches!(
            step(&mut machine, &mut hooks),
            ChunkProgress::More { .. }
        ));
        let exit = machine.exits.get(0).unwrap();
        assert!(matches!(&exit.completion, ExitCompletion::Arguments { .. }));
        assert_eq!(exit.state.phase(), cancellation::ExitPhase::Prepare);
        assert!(exit.retirement_capacity > 0);
        assert_eq!(exit.retirement_capacity, exit.state.retirement_capacity());
        assert_eq!(machine.retirement_capacity, exit.retirement_capacity);
        assert_eq!(
            machine.retirement_bytes,
            machine.retirement_capacity * std::mem::size_of::<TaskRetirementWitness>()
        );
        let metadata = exit.state.context();
        assert_eq!(
            drops.load(Ordering::Acquire),
            0,
            "abandon before any broadcast or teardown"
        );
        let before = runtime.snapshot().unwrap();
        let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(machine)));
        let receipt = rx.try_recv();
        let after_drop = current_cleanup_context();
        let cancelled = runtime.snapshot().unwrap();
        drop(ambient_guard);
        let outside = (current_cleanup_context(), current_execution_termination());
        blocked.release.release();
        healthy_and_drain(&runtime, 3);
        eprintln!("arguments abandon native={panics} actual_cap={capacity} metadata={metadata:?} before={before:?} cancelled={cancelled:?} receipt={receipt:?}");
        let receipt = receipt.unwrap();
        assert!(
            receipt.parent_alive,
            "real staged owner must drop before caller Env prefix"
        );
        assert_eq!(
            receipt.snapshot.pending_tasks, 1,
            "sole primary async Task must still be pending during staged owner's Drop"
        );
        assert_eq!(receipt.snapshot.cancelling_tasks, 1);
        assert_eq!(receipt.context, Some(effective));
        assert_eq!(metadata.unwrap().reason, primary.reason);
        assert_eq!(after_drop, Some(effective));
        assert_eq!(outside, (None, None));
        assert_eq!(
            (
                before.active_tasks,
                before.queued_tasks,
                before.finished_tasks
            ),
            (3, 2, 0)
        );
        assert_eq!(
            (
                cancelled.active_tasks,
                cancelled.queued_tasks,
                cancelled.pending_tasks,
                cancelled.running_tasks,
                cancelled.cancelling_tasks,
                cancelled.registered_tasks,
                cancelled.finished_tasks
            ),
            (3, 2, 0, 1, 0, 3, 0)
        );
        assert_eq!(parent_observer.strong_count(), 0);
        assert_eq!(observer.strong_count(), 0);
        assert_eq!(drops.load(Ordering::Acquire), 1);
        assert!(!ran.load(Ordering::Acquire));
        assert!(hooks.done.is_empty());
        if panics {
            let panic = match dropped {
                Err(panic) => panic,
                Ok(()) => panic!("actual native header abandonment must report panic"),
            };
            assert_eq!(
                panic.downcast_ref::<&str>().copied(),
                Some("actual arguments capture native Drop panic")
            );
        } else {
            assert!(dropped.is_ok());
        }
    }
}

#[test]
fn later_argument_error_retires_the_already_staged_parameter_once() {
    let source = functions(
        r#"
fn First(n: int): int { return n + 1 }
fn Second(): int { return 1 / 0 }
fn Later(): int { return 9223372036854775807 + 1 }
fn Target(a: int, b: int, c: int): int { return a + b + c }
fn Entry(n: int): int { marker = 7
    return Target(First(n), Second(), Later()) }
"#,
    );
    let runtime = TaskRuntime::with_limits(1, 1024, 0, 0, 1024);
    let mut machine = new_machine(&source, false);
    let mut hooks = ArgumentHooks::new(&source, &runtime);
    let span = return_call(&source).span;
    to_marker(&mut machine, &mut hooks, span, false);
    let marker = machine.work.get(machine.work.len() - 1).unwrap();
    let Work::ArgumentsExit(frame) = marker else {
        unreachable!()
    };
    assert_eq!(frame.staged, [Value::Int(3)]);
    assert!(frame.staged.capacity() > 0);
    assert_eq!(scalar(&machine.calls.get(0).unwrap().env, "marker"), 7);
    assert_eq!(
        machine
            .pending
            .as_ref()
            .unwrap()
            .as_ref()
            .err()
            .unwrap()
            .message,
        "division by zero"
    );
    let result = finish(&mut machine, &mut hooks);
    assert_eq!(result.err().unwrap().message, "division by zero");
    assert_eq!(
        hooks
            .prepared
            .iter()
            .filter(|(at, _, _)| *at == span)
            .count(),
        1
    );
    assert!(hooks.prepared.contains(&(span, 0, 1)));
    assert_eq!(
        hooks.done.iter().filter(|(at, _, _)| *at == span).count(),
        1
    );
    for name in ["First", "Second"] {
        assert_eq!(
            hooks
                .trace
                .ticks
                .iter()
                .filter(|at| **at == stmt_span(&source[name].body[0]))
                .count(),
            1
        );
    }
    for name in ["Later", "Target"] {
        assert!(!hooks
            .trace
            .ticks
            .contains(&stmt_span(&source[name].body[0])));
    }
    assert_eq!(
        (machine.metrics.entered_calls, machine.metrics.retired_calls),
        (3, 3)
    );
    assert_eq!((machine.argument_capacity, machine.argument_bytes), (0, 0));
    assert_drained(&machine.metrics, 0);
    drop(machine);
    drain(&runtime, 0);
}

#[test]
fn arguments_fuel_failure_discards_sole_async_task_under_one_deadline() {
    let runtime = TaskRuntime::with_limits(1, 1024, 0, 0, 1024);
    let mut blocked = Blocker::start(&runtime);
    let source = functions(ASYNC);
    let span = async_call_span(&source);
    let mut machine = new_machine(&source, true);
    let mut hooks = ArgumentHooks::new(&source, &runtime);
    hooks.trace.host.async_execution = true;
    to_marker(&mut machine, &mut hooks, span, false);
    assert!(
        matches!(machine.pending.as_ref(), Some(Ok(Flow::Return { value: Value::Task(task), .. })) if task.status() == "pending")
    );
    assert!(matches!(
        step(&mut machine, &mut hooks),
        ChunkProgress::More { .. }
    ));
    let exit = machine.exits.get(0).unwrap();
    assert!(matches!(&exit.completion, ExitCompletion::Arguments { .. }));
    assert_eq!(exit.state.phase(), cancellation::ExitPhase::Prepare);
    assert_eq!(exit.state.context(), None);
    // Exhaust the original private containment budget at this actual owner
    // checkpoint. Mandatory cleanup still runs; neither body nor fuel restarts.
    machine.test_fuel = true;
    machine.remaining_dispatches = 0;
    let result = finish(&mut machine, &mut hooks);
    let error = result
        .err()
        .expect("original containment failure survives cleanup");
    assert_eq!(
        error.message,
        "internal execution prototype test fuel exhausted"
    );
    let context = error
        .scope_cleanup()
        .expect("sole losing Task has one cleanup deadline");
    assert_eq!(context.reason, TerminationReason::Cancelled);
    assert_eq!(machine.fallback_release.get(), Some(context));
    assert_eq!(machine.remaining_dispatches, 0);
    assert!(!hooks
        .trace
        .ticks
        .contains(&stmt_span(&source["Child"].body[0])));
    assert_eq!(machine.calls.len(), 0);
    assert_eq!(
        hooks.done.iter().filter(|(at, _, _)| *at == span).count(),
        1
    );
    assert_eq!(
        hooks.done.iter().find(|(at, _, _)| *at == span).unwrap().1,
        Outgoing::Error
    );
    let before_retirement = runtime.snapshot().unwrap();
    assert_eq!(
        (
            before_retirement.active_tasks,
            before_retirement.queued_tasks,
            before_retirement.cancelled_tasks,
            before_retirement.finished_tasks
        ),
        (1, 0, 0, 1)
    );
    assert_eq!(before_retirement.registered_tasks, 1);
    assert_eq!(
        (current_cleanup_context(), current_execution_termination()),
        (None, None)
    );
    assert_drained(&machine.metrics, 0);
    drop(machine);
    blocked.release.release();
    healthy_and_drain(&runtime, 2);
}
