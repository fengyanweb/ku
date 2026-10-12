use super::*;

const PAYLOAD: &str = "\u{4e2d}\u{e9}\0kept";

const RELAY: &str = r#"
fn Relay(text: str, n: int): str! {
    if (n <= 0) { return ok(text) }
    return Relay(text, n - 1)
}
"#;

const MUTUAL: &str = r#"
fn Left(text: str, n: int): str! {
    if (n <= 0) { return ok(Keep(text)) }
    return Right(text, n - 1)
}
fn Right(text: str, n: int): str! {
    marker = 0
    while (marker < 2) { marker = marker + 1 }
    if (n <= 0) { return ok(Keep(text)) }
    return Left(text, n - 1)
}
fn Keep(text: str): str { return text }
"#;

fn input(n: i64) -> (Vec<Value>, *const u8, usize) {
    let mut text = String::with_capacity(4096);
    text.push_str(PAYLOAD);
    let pointer = text.as_ptr();
    let capacity = text.capacity();
    (vec![Value::String(text), Value::Int(n)], pointer, capacity)
}

fn machine(source: &str, entry: &str, limits: Limits) -> Machine {
    let functions = functions(source);
    let code = CodeHandle::compile_owned_named(&functions, &functions[entry], limits).unwrap();
    Machine::from_code(code, limits, 0)
}

fn check_result(value: &Value, pointer: *const u8, capacity: usize) -> *const Value {
    let Value::Result { ok: true, value } = value else {
        panic!("the real sole success Result must survive: {value:?}");
    };
    let Value::String(text) = value.as_ref() else {
        panic!("the original String must remain the Result leaf");
    };
    assert_eq!(text.as_ptr(), pointer);
    assert_eq!(text.capacity(), capacity);
    assert_eq!(text, PAYLOAD);
    value.as_ref() as *const Value
}

fn observe_box(
    machine: &Machine,
    pointer: *const u8,
    capacity: usize,
    seen: &mut Option<*const Value>,
) -> usize {
    let mut live = 0;
    let mut observe = |value: &Value| {
        if matches!(value, Value::Result { .. }) {
            let current = check_result(value, pointer, capacity);
            if let Some(original) = *seen {
                assert_eq!(current, original, "a relay must move the same real Box");
            } else {
                *seen = Some(current);
            }
            live += 1;
        }
    };
    for value in machine.values.as_slice() {
        observe(value);
    }
    if let Some(Ok(Flow::Return { value, .. } | Flow::Fail { value, .. })) = &machine.pending {
        observe(value);
    }
    for exit in machine.exits.as_slice() {
        if let Some(Ok(Flow::Return { value, .. } | Flow::Fail { value, .. })) =
            exit.state.outcome()
        {
            observe(value);
        }
    }
    assert!(live <= 1, "the admitted tree has one actual outcome owner");
    assert_eq!(machine.result_wrapper_capacity, live);
    assert_eq!(
        machine.result_wrapper_bytes,
        live * std::mem::size_of::<Value>()
    );
    assert_eq!(machine.metrics.retained_result_wrapper_capacity, live);
    assert_eq!(
        machine.metrics.retained_result_wrapper_bytes,
        live * std::mem::size_of::<Value>()
    );
    live
}

fn finish(
    machine: &mut Machine,
    host: &mut Interpreter,
    chunk: usize,
    pointer: *const u8,
    capacity: usize,
) -> KuResult<Value> {
    let mut seen = None;
    let chunk = NonZeroUsize::new(chunk).unwrap();
    for _ in 0..100_000 {
        match machine.dispatch_chunk(chunk, host).unwrap() {
            ChunkProgress::More { popped } => {
                assert_eq!(popped, chunk.get());
                observe_box(machine, pointer, capacity, &mut seen);
            }
            ChunkProgress::Waiting { .. } => panic!("this synchronous slice has no await"),
            ChunkProgress::Finished { popped, result } => {
                assert!(popped <= chunk.get());
                let result =
                    result.and_then(|flow| flow.into_call_value(Span::default(), "function"));
                if let Ok(value) = &result {
                    let outgoing = check_result(value, pointer, capacity);
                    if let Some(original) = seen {
                        assert_eq!(
                            outgoing, original,
                            "the outgoing Box is the same sole owner"
                        );
                    }
                }
                return result;
            }
        }
    }
    panic!("the finite Result String fixture did not finish");
}

fn pause_at(machine: &mut Machine, host: &mut Interpreter, ready: impl Fn(&Machine) -> bool) {
    for _ in 0..100_000 {
        if ready(machine) {
            return;
        }
        match machine
            .dispatch_chunk(NonZeroUsize::new(1).unwrap(), host)
            .unwrap()
        {
            ChunkProgress::More { popped: 1 } => {}
            _ => panic!("the actual requested checkpoint was not reached"),
        }
    }
    panic!("the finite Result String checkpoint was not reached");
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

fn assert_owned_drained(machine: &Machine) {
    assert_eq!((machine.argument_capacity, machine.argument_bytes), (0, 0));
    assert_eq!(
        (
            machine.result_wrapper_capacity,
            machine.result_wrapper_bytes
        ),
        (0, 0)
    );
    assert_eq!(machine.string_payload_bytes, 0);
    assert_eq!(machine.metrics.retained_string_payload_bytes, 0);
    assert_eq!(machine.metrics.retained_result_wrapper_capacity, 0);
    assert_eq!(machine.metrics.retained_result_wrapper_bytes, 0);
    assert!(machine.entry_args.is_none());
    assert_drained(&machine.metrics, 0);
}

#[test]
fn production_owned_result_string_relay_preserves_original_buffer_at_shallow_depth() {
    // Keep this regression independent of the new private machine fields so
    // it also runs against the precise pre-slice production implementation.
    let source = r#"
fn Relay(text: str, n: int): str! {
    if (n <= 0) { return ok(text) }
    return Relay(text, n - 1)
}
"#;
    let mut text = String::with_capacity(4096);
    text.push_str("\u{4fdd}\u{7559}\0payload");
    let pointer = text.as_ptr();
    let capacity = text.capacity();
    let mut interpreter = production_interpreter(source);
    let result = interpreter
        .call_function(
            "Relay",
            vec![Value::String(text), Value::Int(4)],
            Span::default(),
            0,
        )
        .unwrap();
    let Value::Result { ok: true, value } = result else {
        panic!("the real production call must return its success Result");
    };
    let Value::String(text) = *value else {
        panic!("the real success payload must be the input String");
    };
    assert_eq!(
        text.as_ptr(),
        pointer,
        "legacy ok cloned the original String buffer"
    );
    assert_eq!(text.capacity(), capacity);
    assert_eq!(text, "\u{4fdd}\u{7559}\0payload");
    assert_eq!(interpreter.call_depth, 0);
}

#[test]
fn result_string_self_and_mutual_relays_move_one_box_and_buffer_through_512_calls() {
    assert_eq!(MAX_CALL_DEPTH, 512);
    for (source, entry, n) in [(RELAY, "Relay", 511), (MUTUAL, "Left", 510)] {
        for chunk in [1, 7, 256] {
            let mut machine = machine(source, entry, Limits::default());
            let mut host = production_interpreter(source);
            let (mut args, pointer, capacity) = input(n);
            machine
                .start_owned_once(entry, &mut args, 0, false)
                .unwrap();
            assert!(args.is_empty() && args.capacity() == 0);
            let result = finish(&mut machine, &mut host, chunk, pointer, capacity).unwrap();
            check_result(&result, pointer, capacity);
            assert_eq!(machine.metrics.entered_calls, 512);
            assert_eq!(machine.metrics.peak_calls, 512);
            assert_eq!(machine.metrics.peak_string_payload_bytes, capacity);
            assert_eq!(machine.metrics.peak_result_wrapper_capacity, 1);
            assert_eq!(
                machine.metrics.peak_result_wrapper_bytes,
                std::mem::size_of::<Value>()
            );
            assert_owned_drained(&machine);
            assert_eq!(host.call_depth, 0);
            machine.prepare_next_run().unwrap();
            let (mut args, pointer, capacity) = input(0);
            machine
                .start_owned_once(entry, &mut args, 0, false)
                .unwrap();
            check_result(
                &finish(&mut machine, &mut host, 7, pointer, capacity).unwrap(),
                pointer,
                capacity,
            );
            assert_owned_drained(&machine);
        }
    }
    let mut host = production_interpreter(RELAY);
    let (args, pointer, capacity) = input(511);
    let actual = host
        .call_function("Relay", args, Span::default(), 0)
        .unwrap();
    check_result(&actual, pointer, capacity);
    assert_eq!(host.call_depth, 0);
}

#[test]
fn result_string_first_excess_call_and_preentry_payload_budget_do_not_replay() {
    for payload_rejection in [false, true] {
        let mut limits = Limits::default();
        let (mut args, pointer, capacity) = input(if payload_rejection { 0 } else { 512 });
        if payload_rejection {
            limits.values.max_storage_bytes = capacity - 1;
        }
        let mut machine = machine(RELAY, "Relay", limits);
        let mut host = production_interpreter(RELAY);
        let error = machine
            .run_owned_with_hooks("Relay", &mut args, 0, false, &mut host)
            .err()
            .expect("the actual payload/depth boundary must reject");
        if payload_rejection {
            assert!(error
                .message
                .contains("internal execution storage rejected"));
            assert_eq!(machine.metrics.entered_calls, 0);
            assert_eq!(host.steps, 0);
            let Value::String(text) = &args[0] else {
                panic!("entry must not move the input")
            };
            assert_eq!(text.as_ptr(), pointer);
            assert_eq!(text.capacity(), capacity);
            assert_eq!(text, PAYLOAD);
        } else {
            assert_eq!(error.code.as_deref(), Some("call_depth_exceeded"));
            assert_eq!(machine.metrics.entered_calls, 512);
            assert_eq!(machine.metrics.peak_calls, 512);
            assert!(args.is_empty() && args.capacity() == 0);
        }
        assert_eq!(machine.metrics.peak_result_wrapper_capacity, 0);
        assert_eq!(machine.metrics.peak_result_wrapper_bytes, 0);
        assert_owned_drained(&machine);
        let (args, pointer, capacity) = input(0);
        check_result(
            &host
                .call_function("Relay", args, Span::default(), 0)
                .unwrap(),
            pointer,
            capacity,
        );
        assert_eq!(host.call_depth, 0);
    }
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
fn result_wrapper_count_and_bytes_reject_after_operand_move_before_box_creation() {
    for count_rejection in [false, true] {
        let mut machine = machine(RELAY, "Relay", Limits::default());
        let mut host = production_interpreter(RELAY);
        let (mut args, pointer, capacity) = input(0);
        machine
            .start_owned_once("Relay", &mut args, 0, false)
            .unwrap();
        pause_at(&mut machine, &mut host, at_wrap);
        assert!(args.is_empty() && host.steps > 0);
        let Value::String(text) = machine.values.get(machine.values.len() - 1).unwrap() else {
            panic!("ok's real operand has already moved into its output slot");
        };
        assert_eq!(text.as_ptr(), pointer);
        assert_eq!(text.capacity(), capacity);
        assert_eq!(
            (
                machine.result_wrapper_capacity,
                machine.result_wrapper_bytes
            ),
            (0, 0)
        );
        if count_rejection {
            machine.value_limits.max_frames = live_value_count(&machine);
        } else {
            machine.value_limits.max_storage_bytes =
                live_value_bytes(&machine) + std::mem::size_of::<Value>() - 1;
        }
        let error = finish(&mut machine, &mut host, 1, pointer, capacity).unwrap_err();
        assert!(error.message.contains(if count_rejection {
            "FrameLimit"
        } else {
            "StorageLimit"
        }));
        assert_eq!(machine.metrics.entered_calls, 1);
        assert_eq!(machine.metrics.peak_string_payload_bytes, capacity);
        assert_eq!(machine.metrics.peak_result_wrapper_capacity, 0);
        assert_eq!(machine.metrics.peak_result_wrapper_bytes, 0);
        assert_owned_drained(&machine);
        machine.value_limits = Limits::default().values;
        machine.prepare_next_run().unwrap();
        let (mut args, pointer, capacity) = input(0);
        machine
            .start_owned_once("Relay", &mut args, 0, false)
            .unwrap();
        check_result(
            &finish(&mut machine, &mut host, 7, pointer, capacity).unwrap(),
            pointer,
            capacity,
        );
        assert_owned_drained(&machine);
    }
}

#[test]
fn result_string_partial_binding_and_transferred_vec_failure_retire_input_once() {
    let source = "fn Entry(first: str, second: str): str! { return ok(first) }";
    for before_binding in [false, true] {
        let (mut args, _, _) = input(0);
        let mut limits = Limits::default();
        if before_binding {
            limits.values.max_frames = args.capacity();
        }
        let mut machine = machine(source, "Entry", limits);
        let mut host = production_interpreter(source);
        // Deliberately exercise the internal binding failure after the first
        // genuine String transfer. This does not admit an ill-typed source call.
        let error = machine
            .run_owned_with_hooks("Entry", &mut args, 0, false, &mut host)
            .err()
            .expect("the real internal entry failure must escape");
        assert!(args.is_empty() && args.capacity() == 0);
        assert_eq!(host.steps, 0);
        assert_eq!(machine.metrics.entered_calls, usize::from(!before_binding));
        if before_binding {
            assert!(error
                .message
                .contains("internal execution storage rejected"));
        }
        assert_eq!(machine.metrics.peak_result_wrapper_capacity, 0);
        assert_owned_drained(&machine);
        let (mut args, _, _) = input(0);
        args[1] = Value::String(String::from("second"));
        let result = host
            .call_function("Entry", args, Span::default(), 0)
            .unwrap();
        assert!(matches!(result, Value::Result { ok: true, .. }));
        assert_eq!(host.call_depth, 0);
    }
}

#[test]
fn result_string_argument_failure_and_fuel_before_or_after_wrap_drain_real_owners() {
    const FAILURE: &str = r#"
fn Keep(text: str, ignored: int): str { return text }
fn Relay(text: str, n: int): str! {
    if (n <= 0) { return ok(Keep(text, 1 / 0)) }
    return Relay(text, n - 1)
}
"#;
    for source in [FAILURE, RELAY] {
        for after_wrap in [false, true] {
            if source == FAILURE && after_wrap {
                continue;
            }
            let mut machine = machine(source, "Relay", Limits::default());
            let mut host = production_interpreter(source);
            let (mut args, pointer, capacity) = input(3);
            let fuel = source == RELAY;
            machine
                .start_owned_once("Relay", &mut args, 0, fuel)
                .unwrap();
            if fuel {
                pause_at(&mut machine, &mut host, at_wrap);
                if after_wrap {
                    assert!(matches!(
                        machine
                            .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut host)
                            .unwrap(),
                        ChunkProgress::More { popped: 1 }
                    ));
                    let mut seen = None;
                    assert_eq!(observe_box(&machine, pointer, capacity, &mut seen), 1);
                    assert_eq!(machine.result_wrapper_capacity, 1);
                    assert_eq!(machine.result_wrapper_bytes, std::mem::size_of::<Value>());
                }
                // Test harness fuel only; production limits remain unchanged.
                machine.remaining_dispatches = 0;
            }
            let error = finish(&mut machine, &mut host, 1, pointer, capacity).unwrap_err();
            assert_eq!(
                error.message,
                if fuel {
                    "internal execution prototype test fuel exhausted"
                } else {
                    "division by zero"
                }
            );
            assert_eq!(
                machine.metrics.entered_calls, 4,
                "a later argument cannot enter Keep"
            );
            assert_eq!(
                machine.metrics.peak_result_wrapper_capacity,
                usize::from(after_wrap)
            );
            assert_eq!(machine.metrics.peak_string_payload_bytes, capacity);
            assert!(args.is_empty());
            assert_owned_drained(&machine);
            assert!(current_cleanup_context().is_none());
            let mut healthy = production_interpreter(RELAY);
            let (args, pointer, capacity) = input(0);
            check_result(
                &healthy
                    .call_function("Relay", args, Span::default(), 0)
                    .unwrap(),
                pointer,
                capacity,
            );
            assert_eq!(healthy.call_depth, 0);
        }
    }
}

#[test]
fn native_kernel_abandonment_of_leaf_string_and_sole_result_restores_tls() {
    for after_wrap in [false, true] {
        let now = Instant::now();
        let ambient = RuntimeTermination {
            reason: TerminationReason::TimedOut,
            cleanup_deadline: now,
        };
        let shorter = RuntimeTermination {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: now.checked_sub(Duration::from_millis(1)).unwrap(),
        };
        let native = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Kernel abandonment uses ordinary native unwind and the actual
            // Machine drop prefix. String and Box have no fabricated user Drop.
            let _guard = CleanupGuard::enter(ambient);
            let mut machine = machine(RELAY, "Relay", Limits::default());
            let mut host = production_interpreter(RELAY);
            let (mut args, pointer, capacity) = input(0);
            machine
                .start_owned_once("Relay", &mut args, 0, false)
                .unwrap();
            pause_at(&mut machine, &mut host, at_wrap);
            if after_wrap {
                assert!(matches!(
                    machine
                        .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut host)
                        .unwrap(),
                    ChunkProgress::More { popped: 1 }
                ));
                let mut seen = None;
                assert_eq!(observe_box(&machine, pointer, capacity, &mut seen), 1);
                assert_eq!(machine.result_wrapper_capacity, 1);
                assert_eq!(machine.result_wrapper_bytes, std::mem::size_of::<Value>());
            } else {
                let Value::String(text) = machine.values.get(machine.values.len() - 1).unwrap()
                else {
                    panic!("the actual terminal operand must be owned")
                };
                assert_eq!(text.as_ptr(), pointer);
                assert_eq!(text.capacity(), capacity);
                assert_eq!(machine.result_wrapper_capacity, 0);
            }
            assert_eq!(machine.string_payload_bytes, capacity);
            assert!(args.is_empty());
            drop(CleanupGuard::enter(shorter));
            let actual = current_cleanup_context().unwrap();
            assert_eq!(actual.reason, ambient.reason);
            assert_eq!(actual.cleanup_deadline, shorter.cleanup_deadline);
            panic!("actual Result String kernel abandonment");
        }));
        let panic = native.expect_err("the real native kernel panic must escape");
        assert_eq!(
            panic.downcast_ref::<&str>().copied(),
            Some("actual Result String kernel abandonment")
        );
        assert!(current_cleanup_context().is_none());
        assert!(crate::runtime::task::current_execution_termination().is_none());
        let mut healthy = production_interpreter(RELAY);
        let (args, pointer, capacity) = input(0);
        check_result(
            &healthy
                .call_function("Relay", args, Span::default(), 0)
                .unwrap(),
            pointer,
            capacity,
        );
        assert_eq!(healthy.call_depth, 0);
    }
}

#[test]
fn unsupported_result_string_trees_and_other_selectors_leave_inputs_unmoved() {
    for source in [
        "fn Entry(text: str): str! { result = ok(text)\nreturn result }",
        "fn Pass(value: str!): str! { return value }\nfn Entry(text: str): str! { return Pass(ok(text)) }",
        "fn Leaf(text: str): str! { return ok(text) }\nfn Entry(text: str): str! { return ok(Leaf(text)?) }",
        "fn Entry(text: str): str! { return err(text) }",
        "fn Entry(text: str): str! { try { marker = 1 } finally { marker = 2 }\nreturn ok(text) }",
        "fn Leaf(text: str): str! { try { marker = 1 } finally { marker = 2 }\nreturn ok(text) }\nfn Entry(text: str): str! { return Leaf(text) }",
        "fn Entry(text: str): str! { try { marker = 1 } catch (e) { marker = 2 }\nreturn ok(text) }",
        "fn Read(&text: str): int { return text.len() }\nfn Entry(text: str): str! { length = Read(text)\nreturn ok(text) }",
        "fn Entry(text: str): str! { fn Local(value: str): str! { return ok(value) }\nreturn Local(text) }",
        "async fn Entry(text: str): str! { return ok(text) }",
        "async fn Leaf(text: str): str! { return ok(text) }\nasync fn Entry(text: str): str! { return await Leaf(text) }",
    ] {
        let source_functions = functions(source);
        let entry = &source_functions["Entry"];
        assert!(!code::eligible_owned_named(&source_functions, entry, Limits::default()), "{source}");
        assert!(matches!(CodeHandle::compile_owned_named(&source_functions, entry, Limits::default()), Err(AdmissionError::Unsupported(_))), "{source}");
        let mut host = production_interpreter(source);
        let (mut args, pointer, capacity) = input(0);
        args.pop();
        assert!(try_execute_owned(&mut host, entry, &mut args, 0).is_none(), "{source}");
        assert_eq!(host.steps, 0);
        assert_eq!(host.call_depth, 0);
        let Value::String(text) = &args[0] else { panic!("unsupported admission must leave the input owner") };
        assert_eq!(text.as_ptr(), pointer);
        assert_eq!(text.capacity(), capacity);
    }
    let source = functions(RELAY);
    let entry = &source["Relay"];
    assert!(!code::eligible_named(&source, entry, Limits::default()));
    assert!(!code::eligible_borrowed_string_named(
        &source,
        entry,
        Limits::default()
    ));
    assert!(matches!(
        CodeHandle::compile_named(&source, entry, Limits::default()),
        Err(AdmissionError::Unsupported(_))
    ));
    assert!(matches!(
        CodeHandle::compile_borrowed_string_named(&source, entry, Limits::default()),
        Err(AdmissionError::Unsupported(_))
    ));
    let mut host = production_interpreter(RELAY);
    let (mut args, pointer, capacity) = input(0);
    assert!(try_execute(&mut host, entry, &args, 0).is_none());
    assert!(try_execute_borrowed_string(&mut host, entry, &mut args, 0).is_none());
    assert_eq!(host.steps, 0);
    let Value::String(text) = &args[0] else {
        unreachable!()
    };
    assert_eq!((text.as_ptr(), text.capacity()), (pointer, capacity));
}

#[test]
fn old_owned_string_finally_and_scalar_result_entry_keep_their_original_roles() {
    const OLD_STRING: &str =
        "fn Entry(text: str): str { try { marker = 1 } finally { marker = 2 }\nreturn text }";
    let mut machine = machine(OLD_STRING, "Entry", Limits::default());
    let (mut args, pointer, capacity) = input(0);
    args.pop();
    let mut host = production_interpreter(OLD_STRING);
    let flow = machine
        .run_owned_with_hooks("Entry", &mut args, 0, false, &mut host)
        .unwrap();
    let Value::String(text) = flow.into_call_value(Span::default(), "function").unwrap() else {
        panic!("the old String role retains finally")
    };
    assert_eq!((text.as_ptr(), text.capacity()), (pointer, capacity));
    assert_eq!(machine.metrics.entered_finally_frames, 1);
    assert_eq!(machine.metrics.peak_result_wrapper_capacity, 0);
    assert_owned_drained(&machine);
    const SCALAR: &str =
        "fn Entry(n: int): int! { try { return ok(n) } finally { marker = 1 }\nreturn ok(-1) }";
    let source = functions(SCALAR);
    let code = CodeHandle::compile_named(&source, &source["Entry"], Limits::default()).unwrap();
    let mut machine = Machine::from_code(code, Limits::default(), 0);
    let mut host = production_interpreter(SCALAR);
    let actual = machine
        .run_with_hooks("Entry", &[Value::Int(7)], 0, false, &mut host)
        .unwrap()
        .into_call_value(Span::default(), "function")
        .unwrap();
    assert_eq!(
        actual,
        Value::Result {
            ok: true,
            value: Box::new(Value::Int(7))
        }
    );
    assert_eq!(machine.metrics.entered_finally_frames, 1);
    assert_eq!(
        machine.metrics.peak_result_wrapper_capacity, 0,
        "legacy scalar Result keeps its separate existing budget contract"
    );
    assert_drained(&machine.metrics, 0);
}

#[test]
fn result_string_ok_operand_keeps_existing_string_helper_finally_before_single_box() {
    const SOURCE: &str = r#"
fn Keep(text: str): str {
    try { marker = 1 } finally { marker = 2 }
    return text
}
fn Entry(text: str): str! { return ok(Keep(text)) }
"#;
    let mut machine = machine(SOURCE, "Entry", Limits::default());
    let mut host = production_interpreter(SOURCE);
    let (mut args, pointer, capacity) = input(0);
    args.pop();
    machine
        .start_owned_once("Entry", &mut args, 0, false)
        .unwrap();
    pause_at(&mut machine, &mut host, at_wrap);
    assert_eq!(machine.metrics.entered_calls, 2);
    assert_eq!(machine.metrics.entered_finally_frames, 1);
    assert_eq!(machine.metrics.retired_finally_frames, 1);
    assert_eq!(machine.finally.frames.len(), 0);
    assert_eq!(
        machine.result_wrapper_capacity, 0,
        "the String helper runs before the Box exists"
    );
    let Value::String(text) = machine.values.get(machine.values.len() - 1).unwrap() else {
        panic!("the helper must transfer its original buffer")
    };
    assert_eq!((text.as_ptr(), text.capacity()), (pointer, capacity));
    let result = finish(&mut machine, &mut host, 1, pointer, capacity).unwrap();
    check_result(&result, pointer, capacity);
    assert_eq!(machine.metrics.peak_string_payload_bytes, capacity);
    assert_eq!(machine.metrics.peak_result_wrapper_capacity, 1);
    assert_eq!(
        machine.metrics.peak_result_wrapper_bytes,
        std::mem::size_of::<Value>()
    );
    assert_owned_drained(&machine);
    assert_eq!(host.call_depth, 0);
}
