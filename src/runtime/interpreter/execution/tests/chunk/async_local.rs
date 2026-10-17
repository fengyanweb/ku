use super::*;
use crate::runtime::task::{AwaitTicket, LocalTaskRunner, TaskHandle, TaskRuntimeSnapshot};
use std::{ops::ControlFlow, sync::mpsc, thread::ThreadId, time::Duration};

const WAIT: Duration = Duration::from_secs(3);

const CHAIN: &str = r#"
fn Make() {
    OFFSET = 20
    async fn local(n: int): int! {
        if (n <= 0) { return ok(OFFSET) }
        target = local(n - 1)
        return ok((await target)? + 1)
    }
    return local.clone()
}
fn CallSite(n: int) {
    callable = Make()
    return callable(n)
}
"#;

fn outcome(n: i64) -> Value {
    Value::Result {
        ok: true,
        value: Box::new(Value::Int(n)),
    }
}

fn until(mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + WAIT;
    while !predicate() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::yield_now();
    }
    true
}

fn drained(snapshot: &TaskRuntimeSnapshot, finished: usize) {
    assert_eq!(snapshot.active_tasks, 0, "{snapshot:?}");
    assert_eq!(snapshot.queued_tasks, 0, "{snapshot:?}");
    assert_eq!(snapshot.registered_tasks, 0, "{snapshot:?}");
    assert_eq!(snapshot.wait_edges, 0, "{snapshot:?}");
    assert_eq!(snapshot.finished_tasks, finished, "{snapshot:?}");
}

fn factory_value(host: &mut Interpreter) -> Value {
    host.call_function("Make", vec![], Span::default(), 0)
        .unwrap()
}

fn call_span(source: &HashMap<String, FnDecl>) -> Span {
    let Stmt::Return {
        value: Some(value), ..
    } = &source["CallSite"].body[1]
    else {
        panic!("checked source has the real callable invocation");
    };
    assert!(matches!(value.kind, ExprKind::Call { .. }));
    value.span
}

fn call(value: &Value, n: i64, span: Span) -> FunctionValueCall<'_> {
    let mut call = function_value_call(value, n, 0);
    call.span = span;
    call
}

// Wait for actual worker retirement before any host-side public await, so the
// host cannot silently become an additional helper for this single-worker test.
fn observe(
    runtime: &TaskRuntime,
    task: TaskHandle,
    finished: usize,
) -> (KuResult<Value>, TaskRuntimeSnapshot) {
    let retired = until(|| runtime.snapshot().unwrap().finished_tasks == finished);
    let value = retired.then(|| task.await_result());
    drop(task);
    let shutdown = runtime.cancel_all_and_wait(WAIT);
    let snapshot = runtime.snapshot().unwrap();
    shutdown.unwrap();
    assert!(retired, "finite real task did not retire: {snapshot:?}");
    (value.unwrap(), snapshot)
}

#[derive(Debug)]
enum Witness {
    Waiting {
        id: i64,
        thread: ThreadId,
        queued: usize,
        edges: usize,
        waiting: usize,
        moved: bool,
        owned_tasks: usize,
        span: Span,
    },
    Finished {
        id: i64,
        thread: ThreadId,
        steps: usize,
        calls: usize,
        retired: usize,
        remaining: (usize, usize, usize),
    },
}

struct SourceRunner {
    inner: ClaimedFunctionValueTask,
    runtime: TaskRuntime,
    events: mpsc::Sender<Witness>,
    reported: bool,
}

impl LocalTaskRunner for SourceRunner {
    fn dispatch(&mut self, _: NonZeroUsize) -> ControlFlow<KuResult<Value>> {
        let progress = self.inner.dispatch(NonZeroUsize::new(1).unwrap());
        let ClaimedFunctionValueTask::Flat(runner) = &self.inner else {
            panic!("real immutable scalar async function value must select the flat runner");
        };
        let machine = runner.machine.as_ref().expect("claimed source machine");
        if machine.state == MachineState::Waiting && !self.reported {
            self.reported = true;
            let snapshot = self.runtime.snapshot().unwrap();
            let env = &machine.calls.get(machine.calls.len() - 1).unwrap().env;
            let moved = env
                .with_value("target", Span::default(), |value| {
                    Ok(matches!(value, Value::Null))
                })
                .unwrap();
            self.events
                .send(Witness::Waiting {
                    id: crate::runtime::task::current_task_id(),
                    thread: std::thread::current().id(),
                    queued: snapshot.queued_tasks,
                    edges: snapshot.wait_edges,
                    waiting: snapshot.waiting_tasks,
                    moved,
                    owned_tasks: env.all_owned_tasks(Span::default()).unwrap().len(),
                    span: machine.entry_span,
                })
                .unwrap();
        }
        if matches!(progress, ControlFlow::Break(_)) {
            self.events
                .send(Witness::Finished {
                    id: crate::runtime::task::current_task_id(),
                    thread: std::thread::current().id(),
                    steps: runner.interpreter.steps,
                    calls: machine.metrics.entered_calls,
                    retired: machine.metrics.retired_calls,
                    remaining: (
                        machine.metrics.remaining_calls,
                        machine.metrics.remaining_work,
                        machine.metrics.remaining_values,
                    ),
                })
                .unwrap();
        }
        progress
    }

    fn waiting(&mut self) -> Option<&mut AwaitTicket> {
        self.inner.waiting()
    }
}

#[test]
fn real_make_async_local_capture_self_await_uses_waiting_then_resumes_same_claimant() {
    let source = functions(CHAIN);
    let runtime = TaskRuntime::with_limits(1, 1024, 0, 0, 1024);
    let mut producer = Interpreter::new();
    result_entry::install(&mut producer, &source);
    producer.task_runtime = Some(runtime.clone());
    let value = factory_value(&mut producer);
    let span = call_span(&source);
    let invocation = call(&value, 6, span);
    assert!(invocation.is_async);
    assert!(invocation.captures.is_immutable_scalar_binding("OFFSET"));
    assert_eq!(
        invocation.captures.get("OFFSET", span).unwrap(),
        Value::Int(20)
    );
    let steps = producer.steps;
    let prepared = prepare_async_function_value(&producer, invocation);
    assert!(matches!(
        prepared.0,
        PreparedAsyncFunctionValueKind::Flat { .. }
    ));
    assert_eq!(producer.steps, steps, "preparation cannot evaluate a body");
    let child_runtime = runtime.clone();
    let probe_runtime = runtime.clone();
    let template = producer.template();
    let (tx, rx) = mpsc::channel();
    let task = runtime
        .spawn_runner_deferred(|| {
            move || SourceRunner {
                inner: ClaimedFunctionValueTask::new(
                    Interpreter::from_template(template, child_runtime),
                    prepared,
                ),
                runtime: probe_runtime,
                events: tx,
                reported: false,
            }
        })
        .unwrap();
    let id = task.id();
    let waiting = rx.recv_timeout(WAIT);
    let finished = rx.recv_timeout(WAIT);
    let (result, snapshot) = observe(&runtime, task, 7);
    eprintln!("actual async local waiting={waiting:?} finished={finished:?} snapshot={snapshot:?}");
    assert_eq!(result.unwrap(), outcome(26));
    drained(&snapshot, 7);
    let Witness::Waiting {
        id: waiting_id,
        thread,
        queued,
        edges,
        waiting,
        moved,
        owned_tasks,
        span: observed_span,
    } = waiting.unwrap()
    else {
        panic!("first actual checkpoint must be Waiting");
    };
    assert_eq!(waiting_id, id);
    assert_eq!((queued, edges, waiting), (1, 1, 1));
    assert!(moved);
    assert_eq!(owned_tasks, 0, "ticket must be the sole moved target owner");
    assert_eq!(observed_span, span);
    let Witness::Finished {
        id: finished_id,
        thread: finished_thread,
        calls,
        retired,
        remaining,
        steps,
    } = finished.unwrap()
    else {
        panic!("actual parent must complete");
    };
    assert_eq!(finished_id, id);
    assert_eq!(finished_thread, thread);
    assert_eq!((calls, retired, remaining), (1, 1, (0, 0, 0)));
    assert!(steps > 0);

    // Exercise the actual interpreter producer callpoint as well, through a
    // checker-valid source CallSite instead of reconstructing its AST.
    let runtime = TaskRuntime::with_limits(1, 1024, 0, 0, 1024);
    producer.task_runtime = Some(runtime.clone());
    let task = producer
        .call_function("CallSite", vec![Value::Int(6)], Span::default(), 0)
        .unwrap();
    let Value::Task(task) = task else {
        panic!("source invocation must return its real task");
    };
    let (result, snapshot) = observe(&runtime, task, 7);
    assert_eq!(result.unwrap(), outcome(26));
    drained(&snapshot, 7);
}

struct TraceRunner {
    machine: Machine,
    hooks: TraceHooks,
    events: mpsc::Sender<(Vec<Span>, Vec<Event>, usize, Metrics)>,
    span: Span,
}

impl LocalTaskRunner for TraceRunner {
    fn dispatch(&mut self, _: NonZeroUsize) -> ControlFlow<KuResult<Value>> {
        match self
            .machine
            .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut self.hooks)
            .unwrap()
        {
            ChunkProgress::More { .. } | ChunkProgress::Waiting { .. } => ControlFlow::Continue(()),
            ChunkProgress::Finished { result, .. } => {
                self.events
                    .send((
                        std::mem::take(&mut self.hooks.ticks),
                        std::mem::take(&mut self.hooks.events),
                        self.hooks.host.steps,
                        std::mem::take(&mut self.machine.metrics),
                    ))
                    .unwrap();
                ControlFlow::Break(
                    result.and_then(|flow| flow.into_call_value(self.span, "function value")),
                )
            }
        }
    }

    fn waiting(&mut self) -> Option<&mut AwaitTicket> {
        (self.machine.state == MachineState::Waiting)
            .then(|| &mut self.machine.awaiting.as_mut().unwrap().ticket)
    }
}

#[test]
fn async_local_self_callee_tick_and_argument_epilogue_match_the_original_source_call() {
    let source = functions(CHAIN);
    let mut producer = Interpreter::new();
    result_entry::install(&mut producer, &source);
    let value = factory_value(&mut producer);
    let span = call_span(&source);
    let invocation = call(&value, 1, span);
    let Stmt::Assign {
        value: expression, ..
    } = &invocation.body[1]
    else {
        panic!("source target assignment");
    };
    let ExprKind::Call { callee, args } = &expression.kind else {
        panic!("source self invocation");
    };
    let expected_tick_order = [expression.span, callee.span, args[0].span];
    let PreparedAsyncFunctionValueKind::Flat {
        code,
        function,
        captures,
        args,
        span: entry,
    } = prepare_async_function_value(&producer, invocation).0
    else {
        panic!("source admission must be flat");
    };
    let runtime = TaskRuntime::with_limits(1, 1024, 0, 0, 1024);
    let child_runtime = runtime.clone();
    let source_template = producer.template();
    let (tx, rx) = mpsc::channel();
    let task = runtime
        .spawn_runner_deferred(|| {
            move || {
                let mut hooks = TraceHooks::new();
                hooks.host = Interpreter::from_template(source_template, child_runtime);
                let mut machine = Machine::from_code(code, Limits::default(), 0);
                machine.captures = Some(captures);
                machine
                    .start_function_once(function, &args, 0, false, entry)
                    .unwrap();
                TraceRunner {
                    machine,
                    hooks,
                    events: tx,
                    span,
                }
            }
        })
        .unwrap();
    let trace = rx.recv_timeout(WAIT);
    let (result, snapshot) = observe(&runtime, task, 2);
    let (ticks, events, steps, metrics) = trace.unwrap();
    assert_eq!(result.unwrap(), outcome(21));
    drained(&snapshot, 2);
    assert_drained(&metrics, 0);
    let position = ticks
        .iter()
        .position(|tick| *tick == expected_tick_order[0])
        .unwrap();
    assert_eq!(&ticks[position..position + 3], &expected_tick_order);
    assert_eq!(
        ticks
            .iter()
            .filter(|tick| **tick == expected_tick_order[1])
            .count(),
        1
    );
    assert_eq!(events.iter().filter(|event| matches!(event, Event::Arguments(argument_span, _, _) if *argument_span == expected_tick_order[0])).count(), 1);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::Call(call_span, true) if *call_span == span))
            .count(),
        1
    );

    let runtime = TaskRuntime::with_limits(1, 1024, 0, 0, 1024);
    let child_runtime = runtime.clone();
    let legacy_template = producer.template();
    let (tx, rx) = mpsc::channel();
    let legacy = runtime
        .spawn(move || {
            let mut host = Interpreter::from_template(legacy_template, child_runtime);
            let value = factory_value(&mut host);
            host.steps = 0;
            let result = host.call_function_value_direct(call(&value, 1, span), true);
            tx.send((host.steps, host.call_depth)).unwrap();
            result
        })
        .unwrap();
    let legacy_steps = rx.recv_timeout(WAIT);
    let (legacy_result, snapshot) = observe(&runtime, legacy, 2);
    assert_eq!(legacy_result.unwrap(), outcome(21));
    drained(&snapshot, 2);
    assert_eq!(
        legacy_steps.unwrap(),
        (steps, 0),
        "callee tick and unticked await resume must preserve the original root steps"
    );
}

#[test]
fn shared_local_code_child_uses_current_invocation_span_without_changing_the_root_code_span() {
    let source = functions(CHAIN);
    let mut producer = Interpreter::new();
    result_entry::install(&mut producer, &source);
    let value = factory_value(&mut producer);
    let root_span = call_span(&source);
    let PreparedAsyncFunctionValueKind::Flat {
        code,
        function,
        captures,
        ..
    } = prepare_async_function_value(&producer, call(&value, 1, root_span)).0
    else {
        panic!("real prepared source code");
    };
    let assigned = code.statement(
        code.block_statement(code.function(function).body, 1)
            .unwrap(),
    );
    let StmtCode::Assign(_, expression) = assigned.kind else {
        panic!("real lowered source assignment");
    };
    let expression = code.expression(expression);
    assert!(matches!(expression.kind, ExprCode::AsyncSelfStart { .. }));
    let current_span = code.span(expression.span);
    assert_ne!(current_span, root_span);
    let runtime = TaskRuntime::with_limits(1, 1024, 0, 0, 1024);
    let child_runtime = runtime.clone();
    let template = producer.template();
    let (tx, rx) = mpsc::sync_channel(1);
    let task = runtime
        .spawn_runner_deferred(|| {
            move || {
                let runner = ClaimedNamedTask::from_local_code(
                    Interpreter::from_template(template, child_runtime),
                    code,
                    function,
                    captures,
                    vec![Value::Int(0)],
                    expression.span,
                );
                let machine = runner.machine.as_ref().unwrap();
                let frame = machine.calls.get(0).unwrap();
                tx.send((
                    machine.entry_span,
                    machine.code.span(frame.epilogue_span),
                    machine.code.span(machine.code.function(function).span),
                    runner.span,
                    matches!(frame.kind, CallKind::FunctionValue),
                    crate::runtime::task::current_task_id(),
                ))
                .unwrap();
                runner
            }
        })
        .unwrap();
    let id = task.id();
    let receipt = rx.recv_timeout(WAIT);
    let (result, snapshot) = observe(&runtime, task, 1);
    assert_eq!(result.unwrap(), outcome(20));
    drained(&snapshot, 1);
    assert_eq!(
        receipt.unwrap(),
        (
            current_span,
            current_span,
            root_span,
            current_span,
            true,
            id
        )
    );
}

#[test]
fn actual_callsite_async_local_preserves_the_existing_64_helper_boundary() {
    eprintln!(
        "async local actual headers: ClaimedFunctionValueTask={} PreparedAsyncFunctionValue={} OwnedFunctionValueCall={} ClaimedLegacyFunctionValue={} ClaimedRejectedFunctionValue={} MachineResources={} CallFrame={} ArgumentFrame={} AwaitFrame={} (excludes payload/backing/AST/RSS)",
        std::mem::size_of::<ClaimedFunctionValueTask>(),
        std::mem::size_of::<PreparedAsyncFunctionValue>(),
        std::mem::size_of::<OwnedFunctionValueCall>(),
        std::mem::size_of::<ClaimedLegacyFunctionValue>(),
        std::mem::size_of::<ClaimedRejectedFunctionValue>(),
        std::mem::size_of::<MachineResources>(),
        std::mem::size_of::<CallFrame>(),
        std::mem::size_of::<ArgumentFrame>(),
        std::mem::size_of::<AwaitFrame>(),
    );
    let source = functions(CHAIN);
    for (n, finished) in [(64_i64, 65_usize), (65, 66)] {
        let runtime = TaskRuntime::with_limits(1, 1024, 0, 0, 1024);
        let mut producer = Interpreter::new();
        result_entry::install(&mut producer, &source);
        producer.task_runtime = Some(runtime.clone());
        let value = producer
            .call_function("CallSite", vec![Value::Int(n)], Span::default(), 0)
            .unwrap();
        let Value::Task(task) = value else {
            panic!("real source CallSite must return its submitted task");
        };
        // The excess pending await has already submitted its genuine last
        // child. Include that cancelled queue node in actual retirement.
        let (result, snapshot) = observe(&runtime, task, finished);
        eprintln!(
            "actual source async local helper boundary n={n} result={result:?} snapshot={snapshot:?}"
        );
        drained(&snapshot, finished);
        assert_eq!(snapshot.accepted_submissions, finished);
        let result = result.unwrap();
        if n == 64 {
            assert_eq!(result, outcome(84));
        } else {
            let Value::Result { ok: false, value } = result else {
                panic!("the first excess pending await must propagate its Result");
            };
            let Value::Object(fields) = *value else {
                panic!("await_depth must retain its original error object");
            };
            assert!(
                matches!(fields.get("code"), Some(Value::String(code)) if code == "await_depth"),
                "{fields:?}"
            );
        }
    }
}

#[test]
fn async_local_unsuitable_captures_and_calls_remain_whole_legacy_in_the_real_producer() {
    for (label, source, expected, finished) in [
        (
            "mutable capture",
            r#"
fn Make() {
    captured = 10
    async fn local(n: int): int! { return ok(captured + n) }
    captured = 20
    return local.clone()
}
"#,
            22,
            1,
        ),
        (
            "Owned capture",
            r#"
fn Make() {
    PREFIX = "kept"
    async fn local(n: int): int! { return ok(PREFIX.len() + n) }
    return local.clone()
}
"#,
            6,
            1,
        ),
        (
            "captured ok",
            r#"
fn Make() {
    fn Wrap(n: int): int! { return ok(n + 100) }
    ok = Wrap.clone()
    async fn local(n: int): int! { return ok(n) }
    return local.clone()
}
"#,
            102,
            1,
        ),
        (
            "global call",
            r#"
fn Add(n: int): int { return n + 10 }
fn Make() {
    async fn local(n: int): int! { return ok(Add(n) + 1) }
    return local.clone()
}
"#,
            13,
            1,
        ),
        (
            "self shadow",
            r#"
fn Make() {
    async fn local(n: int): int! {
        if (n <= 0) { local: int = 7
            return ok(local) }
        target = local(n - 1)
        return ok((await target)?)
    }
    return local.clone()
}
"#,
            7,
            3,
        ),
        (
            "global name precedence",
            r#"
async fn collide(n: int): int! { return ok(n + 100) }
fn Make() {
    async fn collide(n: int): int! {
        if (n <= 0) { return ok(0) }
        target = collide(n - 1)
        return ok((await target)?)
    }
    return collide.clone()
}
"#,
            101,
            2,
        ),
        (
            "Result local",
            r#"
fn Make() {
    async fn local(n: int): int! { result = ok(n)
        return result }
    return local.clone()
}
"#,
            2,
            1,
        ),
    ] {
        let source = functions(&format!(
            "{source}\nfn CallSite(n: int) {{ callable = Make()\nreturn callable(n) }}"
        ));
        let runtime = TaskRuntime::with_limits(1, 1024, 0, 0, 1024);
        let mut producer = Interpreter::new();
        result_entry::install(&mut producer, &source);
        producer.task_runtime = Some(runtime.clone());
        let value = factory_value(&mut producer);
        let span = call_span(&source);
        let steps = producer.steps;
        assert!(
            !code::eligible_claimed_async_local(
                &producer.functions,
                &call(&value, 2, span),
                Limits::default()
            ),
            "{label}"
        );
        assert!(
            matches!(
                CodeHandle::compile_claimed_async_local(
                    &producer.functions,
                    &call(&value, 2, span),
                    Limits::default()
                ),
                Err(AdmissionError::Unsupported(_))
            ),
            "{label}"
        );
        let prepared = prepare_async_function_value(&producer, call(&value, 2, span));
        assert!(
            matches!(prepared.0, PreparedAsyncFunctionValueKind::Legacy(_)),
            "{label}"
        );
        assert_eq!(
            producer.steps, steps,
            "{label}: admission must not run a body"
        );
        drop(prepared);
        let Value::Task(task) = producer.call_function_value(call(&value, 2, span)).unwrap() else {
            panic!("{label}: producer must return its genuine task");
        };
        let (result, snapshot) = observe(&runtime, task, finished);
        eprintln!("whole legacy async local {label}: result={result:?} snapshot={snapshot:?}");
        assert_eq!(result.unwrap(), outcome(expected), "{label}");
        drained(&snapshot, finished);
    }
}
