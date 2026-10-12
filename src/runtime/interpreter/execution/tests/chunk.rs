use super::*;

mod arguments_exit;
mod async_local;
mod await_entry;
mod await_fuel;
mod call_exit;
mod result_entry;
mod scope_exit;

#[derive(Debug, PartialEq, Eq)]
enum Event {
    Tick(Span),
    Scope(Span, bool),
    Call(Span, bool),
    Arguments(Span, usize, usize),
    PrepareFinally(Span),
    Finally(Span),
}

struct TraceHooks {
    host: Interpreter,
    ticks: Vec<Span>,
    events: Vec<Event>,
    timeout_at: Option<Span>,
    assert_cancelled_at: Option<Span>,
    observed_queued_cancel: bool,
    pending_arguments: Option<(Span, usize, usize)>,
}

impl TraceHooks {
    fn new() -> Self {
        Self {
            host: Interpreter::new(),
            ticks: Vec::new(),
            events: Vec::new(),
            timeout_at: None,
            assert_cancelled_at: None,
            observed_queued_cancel: false,
            pending_arguments: None,
        }
    }
}

impl ExecutionHooks for TraceHooks {
    fn host(&mut self) -> &mut Interpreter {
        &mut self.host
    }

    fn tick(&mut self, span: Span) -> KuResult<()> {
        self.ticks.push(span);
        self.events.push(Event::Tick(span));
        if self.timeout_at == Some(span) {
            self.host.execution_deadline = Some(HttpHandlerDeadline::new(Instant::now()));
            self.timeout_at = None;
        }
        if self.assert_cancelled_at == Some(span) {
            let snapshot = self.host.task_runtime.as_ref().unwrap().snapshot().unwrap();
            // The inherited cause is TimedOut, so the target's state is
            // timed_out rather than the cancelled bucket. It is terminal but
            // its genuine queue node still awaits worker retirement.
            assert_eq!((snapshot.pending_tasks, snapshot.queued_tasks), (0, 1));
            self.observed_queued_cancel = true;
        }
        ExecutionHooks::tick(&mut self.host, span)
    }

    fn finish_scope(
        &mut self,
        env: &mut Env,
        result: KuResult<Flow>,
        span: Span,
        pop: bool,
    ) -> KuResult<Flow> {
        self.events.push(Event::Scope(span, pop));
        ExecutionHooks::finish_scope(&mut self.host, env, result, span, pop)
    }

    fn finish_call(
        &mut self,
        env: Env,
        leases: Vec<BorrowLease>,
        result: KuResult<Flow>,
        span: Span,
        kind: CallKind,
    ) -> KuResult<Flow> {
        self.events
            .push(Event::Call(span, matches!(kind, CallKind::FunctionValue)));
        ExecutionHooks::finish_call(&mut self.host, env, leases, result, span, kind)
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
        self.events
            .push(Event::Call(span, matches!(kind, CallKind::FunctionValue)));
        ExecutionHooks::finish_call_with_feedback(
            &mut self.host,
            env,
            leases,
            result,
            span,
            kind,
            feedback,
        )
    }

    fn finish_call_value(
        &mut self,
        result: KuResult<Flow>,
        span: Span,
        kind: CallKind,
    ) -> KuResult<Flow> {
        self.events
            .push(Event::Call(span, matches!(kind, CallKind::FunctionValue)));
        ExecutionHooks::finish_call_value(&mut self.host, result, span, kind)
    }

    fn finish_arguments(
        &mut self,
        env: &mut Env,
        leases: Vec<BorrowLease>,
        values: Vec<Value>,
        result: KuResult<Flow>,
        span: Span,
    ) -> KuResult<Flow> {
        self.events
            .push(Event::Arguments(span, leases.len(), values.len()));
        ExecutionHooks::finish_arguments(&mut self.host, env, leases, values, result, span)
    }

    fn prepare_arguments_exit(
        &mut self,
        _env: &mut Env,
        leases: &[BorrowLease],
        values: &mut [Value],
        span: Span,
    ) -> KuResult<()> {
        assert!(self.pending_arguments.is_none());
        self.pending_arguments = Some((span, leases.len(), values.len()));
        Ok(())
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
        let observed = self.pending_arguments.take().expect("prepared arguments");
        assert_eq!(observed.0, span);
        self.events
            .push(Event::Arguments(span, leases.len(), values.len()));
        self.host
            .finish_call_arguments_with_feedback(env, leases, values, result, span, feedback)
    }

    fn finish_arguments_value(&mut self, result: KuResult<Flow>, span: Span) -> KuResult<Flow> {
        let (observed_span, leases, values) =
            self.pending_arguments.take().expect("prepared arguments");
        assert_eq!(observed_span, span);
        self.events.push(Event::Arguments(span, leases, values));
        result
    }

    fn prepare_finally(
        &mut self,
        env: &Env,
        primary: &KuResult<Flow>,
        span: Span,
    ) -> cancellation::FinallyMode {
        self.events.push(Event::PrepareFinally(span));
        ExecutionHooks::prepare_finally(&mut self.host, env, primary, span)
    }

    fn finish_finally(
        &mut self,
        primary: KuResult<Flow>,
        replacement: KuResult<Flow>,
        mode: cancellation::FinallyMode,
        span: Span,
    ) -> KuResult<Flow> {
        self.events.push(Event::Finally(span));
        ExecutionHooks::finish_finally(&mut self.host, primary, replacement, mode, span)
    }
}

fn drive_chunks(
    machine: &mut Machine,
    hooks: &mut TraceHooks,
    budget: NonZeroUsize,
) -> (KuResult<Value>, usize) {
    let mut more = 0;
    // A finite fixture containment bound, independent of production semantics.
    for _ in 0..100_000 {
        match machine.dispatch_chunk(budget, hooks).unwrap() {
            ChunkProgress::Waiting { .. } => panic!("this scalar fixture has no await"),
            ChunkProgress::More { popped } => {
                assert_eq!(popped, budget.get());
                assert_eq!(machine.state, MachineState::Running);
                more += 1;
            }
            ChunkProgress::Finished { popped, result } => {
                assert!(popped <= budget.get());
                assert_eq!(machine.state, MachineState::Finished);
                return (
                    result.and_then(|flow| flow.into_call_value(Span::default(), "function")),
                    more,
                );
            }
        }
    }
    panic!("the finite checked chunk fixture did not finish");
}

#[test]
fn chunk_sizes_preserve_512_calls_ticks_and_real_finally_epilogues() {
    const FINALLY: &str = r#"
fn Descend(n: int): int {
    result = 0
    try { if (n > 0) { result = Descend(n - 1) } }
    finally { result = result + 1 }
    return result
}
"#;
    for (text, argument, expected, finally_count) in [
        (RECURSION, 511, 0, 0),
        (FINALLY, 511, 512, 512),
        (FINALLY, 3, 4, 4),
    ] {
        let source = functions(text);
        let mut synchronous = Machine::new(&source, Limits::default(), 0).unwrap();
        let mut original_hooks = TraceHooks::new();
        let original_result = synchronous
            .run_with_hooks(
                "Descend",
                &[Value::Int(argument)],
                0,
                false,
                &mut original_hooks,
            )
            .and_then(|flow| flow.into_call_value(Span::default(), "function"));
        assert_eq!(original_result, Ok(Value::Int(expected)));
        assert_drained(&synchronous.metrics, 0);
        if argument == 3 {
            let mut legacy = production_interpreter(text);
            let entry = legacy.functions.get("Descend").unwrap().clone();
            let result = legacy
                .call_function_direct_flow(entry, vec![Value::Int(argument)], Span::default(), 0)
                .and_then(|flow| flow.into_call_value(Span::default(), "function"));
            assert_eq!(result, original_result);
            assert_eq!(legacy.steps, original_hooks.host.steps);
            assert_eq!(legacy.call_depth, 0);
        }
        for size in [1, 7, 256, 4096] {
            let mut machine = Machine::from_code(synchronous.code.clone(), Limits::default(), 0);
            let mut hooks = TraceHooks::new();
            machine
                .start_once("Descend", &[Value::Int(argument)], 0, false)
                .unwrap();
            let (result, more) =
                drive_chunks(&mut machine, &mut hooks, NonZeroUsize::new(size).unwrap());
            assert_eq!(result, original_result, "budget {size}");
            assert_eq!(hooks.ticks, original_hooks.ticks, "budget {size}");
            assert_eq!(hooks.events, original_hooks.events, "budget {size}");
            assert_eq!(hooks.host.steps, original_hooks.host.steps);
            assert_eq!(machine.metrics.dispatches, synchronous.metrics.dispatches);
            assert_eq!(machine.metrics.steps, synchronous.metrics.steps);
            assert_eq!(machine.metrics.entered_calls, argument as usize + 1);
            assert_eq!(machine.metrics.peak_calls, argument as usize + 1);
            assert_eq!(machine.metrics.entered_finally_frames, finally_count);
            assert_eq!(
                machine.metrics.retained_backing_bytes,
                synchronous.metrics.retained_backing_bytes
            );
            assert_eq!(
                machine.metrics.peak_backing_bytes_upper_bound,
                synchronous.metrics.peak_backing_bytes_upper_bound
            );
            assert_eq!(
                machine.remaining_dispatches,
                synchronous.remaining_dispatches
            );
            assert_drained(&machine.metrics, 0);
            if size == 1 {
                assert!(more > 1, "the fixture must actually resume between chunks");
            }
        }
    }
}

#[test]
fn first_argument_failure_stays_pending_across_chunks_without_later_effects() {
    let source = functions(
        r#"
fn First(): int { return 1 / 0 }
fn Second(): int { return 9223372036854775807 + 1 }
fn Third(n: int): int { return Third(n + 1) }
fn Target(a: int, b: int, c: int): int { return a + b + c }
fn Entry(): int { return Target(First(), Second(), Third(0)) }
"#,
    );
    let mut machine = Machine::new(&source, Limits::default(), 0).unwrap();
    let mut hooks = TraceHooks::new();
    machine.start_once("Entry", &[], 0, true).unwrap();
    let mut pending_checkpoints = 0;
    let mut completed = None;
    for _ in 0..100_000 {
        match machine
            .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut hooks)
            .unwrap()
        {
            ChunkProgress::Waiting { .. } => panic!("this scalar fixture has no await"),
            ChunkProgress::More { popped } => {
                assert_eq!(popped, 1);
                assert_eq!(machine.state, MachineState::Running);
                if let Some(Err(error)) = &machine.pending {
                    assert_eq!(error.message, "division by zero");
                    assert!(machine.work.len() > 0);
                    pending_checkpoints += 1;
                }
                assert!(machine.metrics.entered_calls <= 2);
            }
            ChunkProgress::Finished { popped, result } => {
                assert_eq!(popped, 1);
                completed = Some(result);
                break;
            }
        }
    }
    assert!(
        pending_checkpoints > 1,
        "actual failure unwind must cross More boundaries"
    );
    let error = completed
        .expect("the finite argument failure must complete")
        .err()
        .expect("the original argument error must survive");
    assert_eq!(error.message, "division by zero");
    for name in ["Second", "Third", "Target"] {
        assert!(
            !hooks.ticks.contains(&stmt_span(&source[name].body[0])),
            "{name} executed"
        );
    }
    assert_eq!(machine.metrics.entered_calls, 2);
    assert_eq!(machine.argument_capacity, 0);
    assert_eq!(machine.argument_bytes, 0);
    assert!(machine.pending.is_none());
    assert_drained(&machine.metrics, 0);
}

fn invocation_snapshot(
    machine: &Machine,
) -> (
    MachineState,
    Span,
    [usize; 13],
    bool,
    Option<KuError>,
    String,
) {
    (
        machine.state,
        machine.entry_span,
        [
            machine.calls.len(),
            machine.work.len(),
            machine.values.len(),
            machine.finally.frames.len(),
            machine.call_depth,
            machine.argument_capacity,
            machine.argument_bytes,
            machine.remaining_dispatches,
            machine.calls.capacity(),
            machine.work.capacity(),
            machine.values.capacity(),
            machine.finally.frames.capacity(),
            usize::from(machine.test_fuel),
        ],
        machine.pending.is_some(),
        machine
            .pending
            .as_ref()
            .and_then(|result| result.as_ref().err().cloned()),
        format!("{:?}", machine.metrics),
    )
}

#[test]
fn chunk_lifecycle_rejects_restarts_and_preserves_fuel_through_real_unwind_and_reuse() {
    let source = functions(&format!("{RECURSION}\nfn Healthy(): int {{ return 7 }}"));
    let limits = Limits {
        prototype_dispatch_steps: 32,
        ..Limits::default()
    };
    let mut machine = Machine::new(&source, limits, 0).unwrap();
    let mut hooks = TraceHooks::new();
    assert!(NonZeroUsize::new(0).is_none());
    let before = invocation_snapshot(&machine);
    assert!(machine
        .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut hooks)
        .is_err());
    assert!(machine.prepare_next_run().is_err());
    assert_eq!(invocation_snapshot(&machine), before);
    assert!(hooks.ticks.is_empty());
    machine
        .start_once("Descend", &[Value::Int(8)], 0, true)
        .unwrap();
    let before = invocation_snapshot(&machine);
    assert!(machine.start_once("Healthy", &[], 0, false).is_err());
    assert!(machine.prepare_next_run().is_err());
    assert_eq!(invocation_snapshot(&machine), before);
    match machine
        .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut hooks)
        .unwrap()
    {
        ChunkProgress::Waiting { .. } => panic!("this scalar fixture has no await"),
        ChunkProgress::More { popped } => assert_eq!(popped, 1),
        ChunkProgress::Finished { .. } => panic!("recursion must retain its continuation"),
    }
    assert_eq!(machine.remaining_dispatches, 31);
    let before = invocation_snapshot(&machine);
    assert!(machine.start_once("Healthy", &[], 0, false).is_err());
    assert_eq!(invocation_snapshot(&machine), before);
    let (result, more) = drive_chunks(&mut machine, &mut hooks, NonZeroUsize::new(1).unwrap());
    assert_eq!(
        result.unwrap_err().message,
        "internal execution prototype test fuel exhausted"
    );
    assert!(
        more > 32,
        "fuel failure must still retire actual exit work across chunks"
    );
    assert_eq!(
        machine.metrics.dispatches - machine.metrics.exit_dispatches,
        32
    );
    assert!(
        machine.metrics.exit_dispatches > 0,
        "mandatory exits still dispatch after fuel exhaustion"
    );
    assert_eq!(machine.remaining_dispatches, 0);
    assert_drained(&machine.metrics, 0);
    let before = invocation_snapshot(&machine);
    assert!(machine.start_once("Healthy", &[], 0, false).is_err());
    assert!(machine
        .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut hooks)
        .is_err());
    assert_eq!(invocation_snapshot(&machine), before);
    let capacities = (
        machine.calls.capacity(),
        machine.work.capacity(),
        machine.values.capacity(),
        machine.finally.frames.capacity(),
    );
    let entered = machine.metrics.entered_calls;
    let code_bytes = machine.code.retained_bytes();
    // Exercise the same production wrapper's explicit independent-call restart,
    // preserving existing tests that reuse a truly drained failed Machine.
    let result = machine
        .run_with_hooks("Healthy", &[], 0, false, &mut hooks)
        .and_then(|flow| flow.into_call_value(Span::default(), "function"));
    assert_eq!(result, Ok(Value::Int(7)));
    assert_eq!(machine.remaining_dispatches, 0);
    assert_eq!(machine.metrics.entered_calls, entered + 1);
    assert_eq!(machine.code.retained_bytes(), code_bytes);
    assert_eq!(
        (
            machine.calls.capacity(),
            machine.work.capacity(),
            machine.values.capacity(),
            machine.finally.frames.capacity()
        ),
        capacities
    );
    assert_drained(&machine.metrics, 0);
}
