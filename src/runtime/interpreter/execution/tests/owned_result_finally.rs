use super::*;

const REPLACE: &str = r#"
fn Entry(first: str, second: str, fallback: str): str! {
    try { return ok(first) }
    finally { return ok(second) }
    return ok(fallback)
}
"#;

const NESTED: &str = r#"
fn Entry(first: str, second: str, third: str, fallback: str): str! {
    try { return ok(first) }
    finally {
        try { return ok(second) }
        finally { return ok(third) }
    }
    return ok(fallback)
}
"#;

#[derive(Clone, Copy, Debug)]
struct Buffer {
    pointer: *const u8,
    capacity: usize,
    text: &'static str,
}

fn inputs(count: usize) -> (Vec<Value>, Vec<Buffer>) {
    let descriptors = [
        ("\u{4e2d}\0primary", 1024),
        ("\u{e9}\0second", 4096),
        ("\u{754c}\0third", 2048),
        ("\0fallback", 512),
    ];
    let mut args = Vec::new();
    let mut buffers = Vec::new();
    for (text, capacity) in descriptors.into_iter().take(count) {
        let mut value = String::with_capacity(capacity);
        value.push_str(text);
        buffers.push(Buffer {
            pointer: value.as_ptr(),
            capacity: value.capacity(),
            text,
        });
        args.push(Value::String(value));
    }
    (args, buffers)
}

fn machine(source: &str) -> Machine {
    let functions = functions(source);
    let code = CodeHandle::compile_owned_named(&functions, &functions["Entry"], Limits::default())
        .unwrap();
    Machine::from_code(code, Limits::default(), 0)
}

fn result_buffer(value: &Value, buffers: &[Buffer]) -> (usize, *const Value) {
    let Value::Result { ok: true, value } = value else {
        panic!("the selected outcome must be an actual success Result");
    };
    let Value::String(text) = value.as_ref() else {
        panic!("the Result leaf must be an input-derived String");
    };
    let index = buffers
        .iter()
        .position(|buffer| buffer.pointer == text.as_ptr())
        .expect("finally must move an original String buffer");
    assert_eq!(text.capacity(), buffers[index].capacity);
    assert_eq!(text, buffers[index].text);
    (index, value.as_ref() as *const Value)
}

fn observe(machine: &Machine, buffers: &[Buffer], boxes: &mut [Option<*const Value>]) -> usize {
    let mut pointers = Vec::new();
    let mut value = |value: &Value| {
        if matches!(value, Value::Result { .. }) {
            let (index, pointer) = result_buffer(value, buffers);
            if let Some(previous) = boxes[index] {
                assert_eq!(
                    pointer, previous,
                    "a saved primary cannot be cloned or reboxed"
                );
            } else {
                boxes[index] = Some(pointer);
            }
            assert!(
                !pointers.contains(&pointer),
                "each actual Box has one owning location"
            );
            pointers.push(pointer);
        }
    };
    for operand in machine.values.as_slice() {
        value(operand);
    }
    if let Some(Ok(Flow::Return { value: primary, .. } | Flow::Fail { value: primary, .. })) =
        &machine.pending
    {
        value(primary);
    }
    for exit in machine.exits.as_slice() {
        if let Some(Ok(Flow::Return { value: primary, .. } | Flow::Fail { value: primary, .. })) =
            exit.state.outcome()
        {
            value(primary);
        }
    }
    for frame in machine.finally.frames.as_slice() {
        if let Some(Ok(Flow::Return { value: primary, .. } | Flow::Fail { value: primary, .. })) =
            &frame.primary
        {
            value(primary);
        }
    }
    let count = pointers.len();
    assert_eq!(machine.result_wrapper_capacity, count);
    assert_eq!(
        machine.result_wrapper_bytes,
        count * std::mem::size_of::<Value>()
    );
    assert_eq!(machine.metrics.retained_result_wrapper_capacity, count);
    assert_eq!(
        machine.metrics.retained_result_wrapper_bytes,
        count * std::mem::size_of::<Value>()
    );
    count
}

fn at_wrap(machine: &Machine) -> bool {
    machine.pending.is_none()
        && machine.exits.len() == 0
        && matches!(
            machine
                .work
                .len()
                .checked_sub(1)
                .and_then(|index| machine.work.get(index)),
            Some(Work::WrapTerminalOk(_, true))
        )
}

fn pause_at(
    machine: &mut Machine,
    hooks: &mut impl ExecutionHooks,
    ready: impl Fn(&Machine) -> bool,
) {
    for _ in 0..100_000 {
        if ready(machine) {
            return;
        }
        match machine
            .dispatch_chunk(NonZeroUsize::new(1).unwrap(), hooks)
            .unwrap()
        {
            ChunkProgress::More { popped: 1 } => {}
            _ => panic!("the requested real finally checkpoint was not reached"),
        }
    }
    panic!("the finite finally checkpoint did not finish");
}

fn finish(
    machine: &mut Machine,
    hooks: &mut impl ExecutionHooks,
    chunk: usize,
    buffers: &[Buffer],
    boxes: &mut [Option<*const Value>],
) -> KuResult<Value> {
    let budget = NonZeroUsize::new(chunk).unwrap();
    for _ in 0..100_000 {
        match machine.dispatch_chunk(budget, hooks).unwrap() {
            ChunkProgress::More { popped } => {
                assert_eq!(popped, chunk);
                observe(machine, buffers, boxes);
            }
            ChunkProgress::Waiting { .. } => {
                panic!("this synchronous finally fixture has no await")
            }
            ChunkProgress::Finished { popped, result } => {
                assert!(popped <= chunk);
                let result =
                    result.and_then(|flow| flow.into_call_value(Span::default(), "function"));
                if let Ok(value) = &result {
                    let (index, pointer) = result_buffer(value, buffers);
                    if let Some(previous) = boxes[index] {
                        assert_eq!(
                            pointer, previous,
                            "the actual outgoing Box must be transferred"
                        );
                    }
                }
                return result;
            }
        }
    }
    panic!("the finite finally fixture did not finish");
}

fn drained(machine: &Machine) {
    assert_eq!(
        (
            machine.result_wrapper_capacity,
            machine.result_wrapper_bytes
        ),
        (0, 0)
    );
    assert_eq!(machine.metrics.retained_result_wrapper_capacity, 0);
    assert_eq!(machine.metrics.retained_result_wrapper_bytes, 0);
    assert_eq!(machine.string_payload_bytes, 0);
    assert_eq!(machine.metrics.retained_string_payload_bytes, 0);
    assert_eq!((machine.argument_capacity, machine.argument_bytes), (0, 0));
    assert!(machine.entry_args.is_none());
    assert_drained(&machine.metrics, 0);
}

fn live_value_count(machine: &Machine) -> usize {
    machine.values.capacity()
        + machine.argument_capacity
        + machine.exit_handle_capacity
        + machine.retirement_capacity
        + machine.lease_capacity
        + machine.lease_root_bytes / std::mem::size_of::<Value>()
        + machine.result_wrapper_capacity
}

fn live_value_bytes(machine: &Machine) -> usize {
    machine.values.charged_storage_bytes()
        + machine.argument_bytes
        + machine.exit_handle_bytes
        + machine.retirement_bytes
        + machine.string_payload_bytes
        + machine.lease_bytes
        + machine.lease_root_bytes
        + machine.result_wrapper_bytes
}

#[test]
fn production_result_string_finally_replacement_preserves_second_original_buffer() {
    // This production regression also compiles against the exact preceding
    // commit; it uses no private fields introduced by the finally slice.
    let source = r#"
fn Entry(first: str, second: str, fallback: str): str! {
    try { return ok(first) }
    finally { return ok(second) }
    return ok(fallback)
}
"#;
    let mut second = String::with_capacity(4096);
    second.push_str("\u{4fdd}\u{7559}\0replacement");
    let pointer = second.as_ptr();
    let capacity = second.capacity();
    let mut interpreter = production_interpreter(source);
    let result = interpreter
        .call_function(
            "Entry",
            vec![
                Value::String(String::from("first")),
                Value::String(second),
                Value::String(String::from("fallback")),
            ],
            Span::default(),
            0,
        )
        .unwrap();
    let Value::Result { ok: true, value } = result else {
        panic!("finally must return the real success replacement");
    };
    let Value::String(second) = *value else {
        panic!("finally must preserve the actual second String owner");
    };
    assert_eq!(
        second.as_ptr(),
        pointer,
        "legacy finally cloned the replacement String buffer"
    );
    assert_eq!(second.capacity(), capacity);
    assert_eq!(second, "\u{4fdd}\u{7559}\0replacement");
    assert_eq!(interpreter.call_depth, 0);
}

#[test]
fn finally_primary_and_replacement_boxes_coexist_and_transfer_original_buffers_across_chunks() {
    for chunk in [1, 7, 256] {
        let mut machine = machine(REPLACE);
        let mut host = production_interpreter(REPLACE);
        let (mut args, buffers) = inputs(3);
        let payload = buffers.iter().map(|buffer| buffer.capacity).sum::<usize>();
        let mut boxes = vec![None; buffers.len()];
        machine
            .start_owned_once("Entry", &mut args, 0, false)
            .unwrap();
        pause_at(&mut machine, &mut host, |machine| {
            machine.result_wrapper_capacity == 2
        });
        assert_eq!(observe(&machine, &buffers, &mut boxes), 2);
        assert!(boxes[0].is_some() && boxes[1].is_some());
        assert_ne!(boxes[0], boxes[1]);
        assert!(
            machine
                .finally
                .frames
                .as_slice()
                .iter()
                .any(|frame| matches!(
                    &frame.primary,
                    Some(Ok(Flow::Return {
                        value: Value::Result { .. },
                        ..
                    }))
                )),
            "the actual first Box is saved in a finally frame"
        );
        assert_eq!(machine.string_payload_bytes, payload);
        let result = finish(&mut machine, &mut host, chunk, &buffers, &mut boxes).unwrap();
        let (index, pointer) = result_buffer(&result, &buffers);
        assert_eq!(index, 1);
        assert_eq!(Some(pointer), boxes[1]);
        assert_eq!(machine.metrics.peak_result_wrapper_capacity, 2);
        assert_eq!(
            machine.metrics.peak_result_wrapper_bytes,
            2 * std::mem::size_of::<Value>()
        );
        assert_eq!(machine.metrics.peak_string_payload_bytes, payload);
        assert_eq!(machine.metrics.entered_finally_frames, 1);
        assert!(args.is_empty() && args.capacity() == 0);
        drained(&machine);
        assert_eq!(host.call_depth, 0);
        machine.prepare_next_run().unwrap();
        let (mut args, buffers) = inputs(3);
        let mut boxes = vec![None; buffers.len()];
        machine
            .start_owned_once("Entry", &mut args, 0, false)
            .unwrap();
        assert_eq!(
            result_buffer(
                &finish(&mut machine, &mut host, chunk, &buffers, &mut boxes).unwrap(),
                &buffers
            )
            .0,
            1
        );
        drained(&machine);
    }
}

#[test]
fn nested_result_finally_retains_three_distinct_boxes_until_actual_arbitration() {
    eprintln!("Result String Box backing={} FinallyFrame header={} (allocator metadata, source and RSS excluded)",
        std::mem::size_of::<Value>(), std::mem::size_of::<FinallyFrame>());
    for chunk in [1, 7, 256] {
        let mut machine = machine(NESTED);
        let mut host = production_interpreter(NESTED);
        let (mut args, buffers) = inputs(4);
        let payload = buffers.iter().map(|buffer| buffer.capacity).sum::<usize>();
        let mut boxes = vec![None; buffers.len()];
        machine
            .start_owned_once("Entry", &mut args, 0, false)
            .unwrap();
        pause_at(&mut machine, &mut host, |machine| {
            machine.result_wrapper_capacity == 3
        });
        assert_eq!(observe(&machine, &buffers, &mut boxes), 3);
        assert_eq!(machine.finally.frames.len(), 2);
        assert!(boxes[..3].iter().all(Option::is_some));
        assert_ne!(boxes[0], boxes[1]);
        assert_ne!(boxes[0], boxes[2]);
        assert_ne!(boxes[1], boxes[2]);
        let result = finish(&mut machine, &mut host, chunk, &buffers, &mut boxes).unwrap();
        assert_eq!(result_buffer(&result, &buffers).0, 2);
        assert_eq!(machine.metrics.peak_result_wrapper_capacity, 3);
        assert_eq!(
            machine.metrics.peak_result_wrapper_bytes,
            3 * std::mem::size_of::<Value>()
        );
        assert_eq!(machine.metrics.peak_string_payload_bytes, payload);
        assert_eq!(machine.metrics.entered_finally_frames, 2);
        assert_eq!(machine.metrics.retired_finally_frames, 2);
        drained(&machine);
        assert!(current_cleanup_context().is_none());
    }
}

#[test]
fn second_result_box_budget_failure_keeps_saved_primary_charge_until_real_discard() {
    for count_rejection in [false, true] {
        let mut machine = machine(REPLACE);
        let mut host = production_interpreter(REPLACE);
        let (mut args, buffers) = inputs(3);
        let mut boxes = vec![None; buffers.len()];
        machine
            .start_owned_once("Entry", &mut args, 0, false)
            .unwrap();
        pause_at(&mut machine, &mut host, |machine| {
            at_wrap(machine) && machine.result_wrapper_capacity == 1
        });
        assert_eq!(observe(&machine, &buffers, &mut boxes), 1);
        let primary = boxes[0].expect("the first real Box is still owned");
        let Value::String(second) = machine.values.get(machine.values.len() - 1).unwrap() else {
            panic!("the replacement operand has already moved before Box admission");
        };
        assert_eq!(
            (second.as_ptr(), second.capacity()),
            (buffers[1].pointer, buffers[1].capacity)
        );
        if count_rejection {
            machine.value_limits.max_frames = live_value_count(&machine);
        } else {
            machine.value_limits.max_storage_bytes =
                live_value_bytes(&machine) + std::mem::size_of::<Value>() - 1;
        }
        assert!(matches!(
            machine
                .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut host)
                .unwrap(),
            ChunkProgress::More { popped: 1 }
        ));
        assert!(
            matches!(&machine.pending, Some(Err(error)) if error.message.contains(if count_rejection { "FrameLimit" } else { "StorageLimit" }))
        );
        assert_eq!(
            observe(&machine, &buffers, &mut boxes),
            1,
            "failed candidate cannot clear the older primary's actual charge"
        );
        assert_eq!(boxes[0], Some(primary));
        assert!(
            boxes[1].is_none(),
            "the rejected second Box was never created"
        );
        assert_eq!(machine.metrics.peak_result_wrapper_capacity, 1);
        let error = finish(&mut machine, &mut host, 1, &buffers, &mut boxes).unwrap_err();
        assert!(error.message.contains(if count_rejection {
            "FrameLimit"
        } else {
            "StorageLimit"
        }));
        assert_eq!(
            machine.metrics.peak_result_wrapper_bytes,
            std::mem::size_of::<Value>()
        );
        drained(&machine);
        machine.value_limits = Limits::default().values;
        machine.prepare_next_run().unwrap();
        let (mut args, buffers) = inputs(3);
        let mut boxes = vec![None; buffers.len()];
        machine
            .start_owned_once("Entry", &mut args, 0, false)
            .unwrap();
        assert_eq!(
            result_buffer(
                &finish(&mut machine, &mut host, 7, &buffers, &mut boxes).unwrap(),
                &buffers
            )
            .0,
            1
        );
        drained(&machine);
    }
}

#[test]
fn finally_continue_keeps_primary_error_replaces_it_and_ordinary_fatal_skips_body() {
    for (source, expected_error, peak_boxes) in [
        ("fn Entry(first: str, second: str, fallback: str): str! { try { return ok(first) } finally { marker = 1 }\nreturn ok(fallback) }", None, 1),
        ("fn Entry(first: str, second: str, fallback: str): str! { try { return ok(first) } finally { marker = 1 / 0 }\nreturn ok(fallback) }", Some("division by zero"), 1),
        ("fn Entry(first: str, second: str, fallback: str): str! { try { marker = 1 / 0 } finally { return ok(second) }\nreturn ok(fallback) }", Some("division by zero"), 0),
    ] {
        for chunk in [1, 7, 256] {
            let mut machine = machine(source);
            let mut host = production_interpreter(source);
            let (mut args, buffers) = inputs(3);
            let mut boxes = vec![None; buffers.len()];
            machine.start_owned_once("Entry", &mut args, 0, false).unwrap();
            let result = finish(&mut machine, &mut host, chunk, &buffers, &mut boxes);
            if let Some(message) = expected_error {
                assert_eq!(result.unwrap_err().message, message);
            } else {
                assert_eq!(result_buffer(&result.unwrap(), &buffers).0, 0);
            }
            assert_eq!(machine.metrics.peak_result_wrapper_capacity, peak_boxes);
            assert_eq!(machine.metrics.entered_finally_frames, 1);
            assert_eq!(machine.metrics.retired_finally_frames, 1);
            drained(&machine);
            let (args, _) = inputs(3);
            let entry = host.functions["Entry"].clone();
            let mut legacy = production_interpreter(source);
            let expected = legacy.call_function_direct_flow(entry, args, Span::default(), 0)
                .and_then(|flow| flow.into_call_value(Span::default(), "function"));
            if let Some(message) = expected_error { assert_eq!(expected.unwrap_err().message, message); }
            else { assert!(matches!(expected, Ok(Value::Result { ok: true, .. }))); }
            assert_eq!(host.steps, legacy.steps, "the same syntax ticks and fatal-skip boundary must remain");
            assert_eq!(host.call_depth, 0);
        }
    }
}

#[test]
fn protected_inner_finally_discards_replacement_box_keeps_first_cause_and_minimum_deadline() {
    const SOURCE: &str = r#"
fn Entry(first: str, second: str, fallback: str): str! {
    try { return ok(first) }
    finally {
        try { marker = 1 }
        finally { return ok(second) }
    }
    return ok(fallback)
}
"#;
    for actual_http_timeout in [false, true] {
        let source = functions(SOURCE);
        let Stmt::Try {
            finally_body: outer,
            ..
        } = &source["Entry"].body[0]
        else {
            unreachable!()
        };
        let Stmt::Try { body: inner, .. } = &outer[0] else {
            unreachable!()
        };
        let execution = ExecutionTerminationGuard::enter();
        let first = RuntimeTermination {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: Instant::now() + HTTP_HANDLER_CLEANUP_GRACE,
        };
        let action = if actual_http_timeout {
            FinallyProbeAction::HttpTimeout
        } else {
            FinallyProbeAction::Terminate(first)
        };
        let mut host = production_interpreter(SOURCE);
        let mut hooks = FinallyProbe::new(&mut host, vec![(stmt_span(&inner[0]), 1, action)]);
        let mut machine = machine(SOURCE);
        let (mut args, buffers) = inputs(3);
        let mut boxes = vec![None; buffers.len()];
        machine
            .start_owned_once("Entry", &mut args, 0, false)
            .unwrap();
        pause_at(&mut machine, &mut hooks, |machine| {
            machine.finally.active_protected.is_some()
        });
        let original = hooks.protected[0];
        assert_eq!(
            original.reason,
            if actual_http_timeout {
                TerminationReason::TimedOut
            } else {
                first.reason
            }
        );
        if !actual_http_timeout {
            assert_eq!(original.cleanup_deadline, first.cleanup_deadline);
        } else {
            assert_eq!(
                hooks
                    .host
                    .execution_deadline
                    .as_ref()
                    .unwrap()
                    .cleanup_deadline,
                Some(original.cleanup_deadline)
            );
        }
        assert_eq!(
            observe(&machine, &buffers, &mut boxes),
            1,
            "outer ordinary primary is still genuinely owned"
        );
        let shorter = RuntimeTermination {
            reason: TerminationReason::TimedOut,
            cleanup_deadline: original
                .cleanup_deadline
                .checked_sub(Duration::from_millis(1))
                .unwrap(),
        };
        let ambient = CleanupGuard::enter(shorter);
        pause_at(&mut machine, &mut hooks, |machine| {
            machine.result_wrapper_capacity == 2
        });
        assert_eq!(observe(&machine, &buffers, &mut boxes), 2);
        assert!(boxes[0].is_some() && boxes[1].is_some());
        assert_eq!(
            current_cleanup_context(),
            Some(shorter),
            "chunk windows must restore ambient TLS"
        );
        let protected = machine.finally.context().unwrap();
        assert_eq!(
            protected.reason, original.reason,
            "protected metadata keeps the first cause"
        );
        assert_eq!(protected.cleanup_deadline, shorter.cleanup_deadline);
        let error = finish(&mut machine, &mut hooks, 1, &buffers, &mut boxes).unwrap_err();
        let context = error
            .runtime_termination()
            .expect("the actual typed termination must survive both arbitrations");
        assert_eq!(context.reason, original.reason);
        assert_eq!(context.cleanup_deadline, shorter.cleanup_deadline);
        assert_eq!(hooks.protected, vec![original]);
        assert!(hooks.triggers.is_empty());
        assert_eq!(machine.metrics.peak_result_wrapper_capacity, 2);
        drained(&machine);
        drop(ambient);
        drop(hooks);
        drop(execution);
        assert!(current_cleanup_context().is_none());
        assert!(crate::runtime::task::current_execution_termination().is_none());
        let mut healthy = production_interpreter(REPLACE);
        let (args, buffers) = inputs(3);
        assert_eq!(
            result_buffer(
                &healthy
                    .call_function("Entry", args, Span::default(), 0)
                    .unwrap(),
                &buffers
            )
            .0,
            1
        );
        assert_eq!(healthy.call_depth, 0);
    }
}

#[test]
fn recursive_result_finally_keeps_full_512_call_boundary_and_first_513_failure() {
    const SOURCE: &str = r#"
fn Entry(text: str, n: int): str! {
    try {
        if (n > 0) { return Entry(text, n - 1) }
    } finally { marker = 1 }
    return ok(text)
}
"#;
    assert_eq!(MAX_CALL_DEPTH, 512);
    for n in [511, 512] {
        for chunk in [1, 7, 256] {
            let mut machine = machine(SOURCE);
            let mut host = production_interpreter(SOURCE);
            let (mut args, buffers) = inputs(1);
            args.push(Value::Int(n));
            let mut boxes = vec![None; buffers.len()];
            machine
                .start_owned_once("Entry", &mut args, 0, false)
                .unwrap();
            let result = finish(&mut machine, &mut host, chunk, &buffers, &mut boxes);
            if n == 511 {
                assert_eq!(result_buffer(&result.unwrap(), &buffers).0, 0);
                assert_eq!(machine.metrics.peak_result_wrapper_capacity, 1);
            } else {
                assert_eq!(
                    result.unwrap_err().code.as_deref(),
                    Some("call_depth_exceeded")
                );
                assert_eq!(machine.metrics.peak_result_wrapper_capacity, 0);
            }
            assert_eq!(machine.metrics.entered_calls, 512);
            assert_eq!(machine.metrics.peak_calls, 512);
            assert_eq!(machine.metrics.entered_finally_frames, 512);
            assert_eq!(machine.metrics.peak_finally_frames, 512);
            assert_eq!(machine.metrics.retired_finally_frames, 512);
            assert_eq!(
                machine.metrics.peak_string_payload_bytes,
                buffers[0].capacity
            );
            assert!(args.is_empty());
            drained(&machine);
            machine.prepare_next_run().unwrap();
            let (mut args, buffers) = inputs(1);
            args.push(Value::Int(0));
            let mut boxes = vec![None; buffers.len()];
            machine
                .start_owned_once("Entry", &mut args, 0, false)
                .unwrap();
            assert_eq!(
                result_buffer(
                    &finish(&mut machine, &mut host, chunk, &buffers, &mut boxes).unwrap(),
                    &buffers
                )
                .0,
                0
            );
            drained(&machine);
        }
        let mut host = production_interpreter(SOURCE);
        let (mut args, buffers) = inputs(1);
        args.push(Value::Int(n));
        let result = host.call_function("Entry", args, Span::default(), 0);
        if n == 511 {
            assert_eq!(result_buffer(&result.unwrap(), &buffers).0, 0);
        } else {
            assert_eq!(
                result.unwrap_err().code.as_deref(),
                Some("call_depth_exceeded")
            );
        }
        assert_eq!(host.call_depth, 0);
    }
}

#[test]
fn native_kernel_abandonment_with_three_real_result_boxes_restores_tls() {
    let native = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut machine = machine(NESTED);
        let mut host = production_interpreter(NESTED);
        let (mut args, buffers) = inputs(4);
        let mut boxes = vec![None; buffers.len()];
        machine
            .start_owned_once("Entry", &mut args, 0, false)
            .unwrap();
        pause_at(&mut machine, &mut host, |machine| {
            machine.result_wrapper_capacity == 3
        });
        assert_eq!(observe(&machine, &buffers, &mut boxes), 3);
        assert_eq!(machine.finally.frames.len(), 2);
        assert_eq!(
            machine.string_payload_bytes,
            buffers.iter().map(|buffer| buffer.capacity).sum::<usize>()
        );
        let original = RuntimeTermination {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: Instant::now(),
        };
        let shorter = RuntimeTermination {
            reason: TerminationReason::TimedOut,
            cleanup_deadline: original
                .cleanup_deadline
                .checked_sub(Duration::from_millis(1))
                .unwrap(),
        };
        let _guard = CleanupGuard::enter(original);
        drop(CleanupGuard::enter(shorter));
        let effective = current_cleanup_context().unwrap();
        assert_eq!(effective.reason, original.reason);
        assert_eq!(effective.cleanup_deadline, shorter.cleanup_deadline);
        // Move the actual owning machine after the guard declaration so its
        // native drop prefix runs before that guard restores TLS. This is a
        // kernel panic, not evidence of a source-level String/Box destructor.
        let _abandoned = machine;
        panic!("actual nested Result finally kernel abandonment");
    }));
    let panic = native.expect_err("the actual kernel panic must escape");
    assert_eq!(
        panic.downcast_ref::<&str>().copied(),
        Some("actual nested Result finally kernel abandonment")
    );
    assert!(current_cleanup_context().is_none());
    assert!(crate::runtime::task::current_execution_termination().is_none());
    let mut healthy = production_interpreter(NESTED);
    let (args, buffers) = inputs(4);
    assert_eq!(
        result_buffer(
            &healthy
                .call_function("Entry", args, Span::default(), 0)
                .unwrap(),
            &buffers
        )
        .0,
        2
    );
    assert_eq!(healthy.call_depth, 0);
}
