use super::async_result_string::{
    drained as errors_drained, error_backing, error_payload, observe as observe_errors,
    result_buffer as outcome_buffer, string_input, Buffer, ErrorPayload,
};
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
        + machine.error_leaf_capacity
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
        + machine.error_leaf_bytes
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
        "fn Entry(text: str): str! { result = err(text)\nreturn result }",
        "fn err(text: str): str! { return ok(text) }\nfn Entry(text: str): str! { return err(text) }",
        "fn Pass(value: str!): str! { return value }\nfn Entry(text: str): str! { return Pass(ok(text)) }",
        "fn Leaf(text: str): str! { return ok(text) }\nfn Entry(text: str): str! { return ok(Leaf(text)?) }",
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
    // Wrong builtin arity is a checker rejection, rather than a legal
    // whole-tree admission fixture. Preserve both sync and async evidence.
    for prefix in ["", "async "] {
        let source = format!(
            "{prefix}fn Entry(text: str): str! {{ return err(text, text) }}\nfn main() {{}}"
        );
        let program = Parser::new(Lexer::new(&source).tokenize().unwrap())
            .parse_program()
            .unwrap();
        let error = crate::checker::Checker::new()
            .check(&program)
            .expect_err("the original checker rejects err's wrong arity");
        assert!(
            error.message.contains("expects 1") && error.message.contains("got 2"),
            "{error:?}"
        );
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

#[test]
fn result_string_no_catch_finally_in_root_or_dependency_preserves_buffer() {
    for source in [
        "fn Entry(text: str): str! { try { marker = 1 } finally { marker = 2 }\nreturn ok(text) }",
        "fn Leaf(text: str): str! { try { marker = 1 } finally { marker = 2 }\nreturn ok(text) }\nfn Entry(text: str): str! { return Leaf(text) }",
    ] {
        let source_functions = functions(source);
        assert!(code::eligible_owned_named(&source_functions, &source_functions["Entry"], Limits::default()));
        let mut machine = machine(source, "Entry", Limits::default());
        let mut host = production_interpreter(source);
        let (mut args, pointer, capacity) = input(0);
        args.pop();
        machine.start_owned_once("Entry", &mut args, 0, false).unwrap();
        let result = finish(&mut machine, &mut host, 1, pointer, capacity).unwrap();
        check_result(&result, pointer, capacity);
        assert_eq!(machine.metrics.entered_finally_frames, 1);
        assert_eq!(machine.metrics.retired_finally_frames, 1);
        assert_eq!(machine.metrics.peak_string_payload_bytes, capacity);
        assert_eq!(machine.metrics.peak_result_wrapper_capacity, 1);
        assert_owned_drained(&machine);
        assert_eq!(host.call_depth, 0);
    }
}

fn at_error_wrap(machine: &Machine) -> bool {
    machine.pending.is_none()
        && machine.exits.len() == 0
        && matches!(
            machine.work.get(machine.work.len() - 1),
            Some(Work::WrapTerminalErr(_))
        )
}

fn finish_error(
    machine: &mut Machine,
    host: &mut Interpreter,
    chunk: usize,
    buffers: &[Buffer],
    boxes: &mut [Option<usize>],
    errors: &mut Vec<ErrorPayload>,
) -> KuResult<Value> {
    let pool = buffers.iter().map(|buffer| buffer.capacity).sum::<usize>();
    let budget = NonZeroUsize::new(chunk).unwrap();
    for _ in 0..100_000 {
        match machine.dispatch_chunk(budget, host).unwrap() {
            ChunkProgress::More { popped } => {
                assert_eq!(popped, chunk);
                assert_eq!(
                    machine.string_payload_bytes, pool,
                    "original input capacity remains reserved even after err clones it"
                );
                observe_errors(machine, buffers, boxes, errors);
            }
            ChunkProgress::Waiting { .. } => panic!("the synchronous err tree has no await"),
            ChunkProgress::Finished { popped, result } => {
                assert!(popped <= chunk);
                let result =
                    result.and_then(|flow| flow.into_call_value(Span::default(), "function"));
                if let Ok(value) = &result {
                    let (index, boxed) = outcome_buffer(value, buffers);
                    if let Some(previous) = boxes[index] {
                        assert_eq!(boxed, previous);
                    }
                    if let Some(actual) = error_payload(value) {
                        if let Some(previous) =
                            errors.iter().find(|previous| previous.boxed == boxed)
                        {
                            assert_eq!(
                                actual, *previous,
                                "the outgoing ErrorObject keeps its actual Map and every buffer"
                            );
                        }
                    }
                }
                return result;
            }
        }
    }
    panic!("the finite terminal err tree did not finish");
}

#[test]
fn terminal_err_sync_input_and_helper_keep_clone_contract_and_real_512_513_boundaries() {
    const DIRECT: &str = "fn Entry(text: str, n: int): str! { return err(text) }";
    const HELPER: &str = "fn Keep(text: str): str { try { marker = 1 } finally { marker = 2 }\nreturn text }\nfn Entry(text: str, n: int): str! { return err(Keep(text)) }";
    const RELAY: &str = "fn Entry(text: str, n: int): str! { if (n <= 0) { return err(text) }\nreturn Entry(text, n - 1) }";
    const MUTUAL: &str = "fn Keep(text: str): str { return text }\nfn Other(text: str, n: int): str! { if (n <= 0) { return err(Keep(text)) }\nreturn Entry(text, n - 1) }\nfn Entry(text: str, n: int): str! { if (n <= 0) { return err(Keep(text)) }\nreturn Other(text, n - 1) }";
    // This remains a legacy compatibility assertion, never the S4 RED:
    // the input is still physically alive while the builtin clones it.
    let (value, original) = string_input(PAYLOAD, 4096);
    let original = [original];
    let args = [value];
    let legacy = crate::stdlib::core::eval_builtin("err", &args, Span::default())
        .unwrap()
        .unwrap();
    let actual = error_payload(&legacy).unwrap();
    assert_ne!(actual.buffers[5].0, original[0].pointer);
    outcome_buffer(&legacy, &original);
    let Value::String(still_live) = &args[0] else {
        unreachable!()
    };
    assert_eq!(
        (still_live.as_ptr() as usize, still_live.capacity()),
        (original[0].pointer, original[0].capacity)
    );

    for (source, n, calls, finalies) in [
        (DIRECT, 0, 1, 0),
        (HELPER, 0, 2, 1),
        (RELAY, 511, 512, 0),
        (MUTUAL, 510, 512, 0),
    ] {
        let source_functions = functions(source);
        assert!(code::eligible_owned_named(
            &source_functions,
            &source_functions["Entry"],
            Limits::default()
        ));
        for chunk in [1, 7, 256] {
            let mut machine = machine(source, "Entry", Limits::default());
            let mut host = production_interpreter(source);
            let (value, buffer) = string_input(PAYLOAD, 4096);
            let buffers = [buffer];
            let mut args = vec![value, Value::Int(n)];
            let mut boxes = [None];
            let mut errors = Vec::new();
            machine
                .start_owned_once("Entry", &mut args, 0, false)
                .unwrap();
            let result = finish_error(
                &mut machine,
                &mut host,
                chunk,
                &buffers,
                &mut boxes,
                &mut errors,
            )
            .unwrap();
            let actual = error_payload(&result).unwrap();
            assert_eq!(outcome_buffer(&result, &buffers).0, 0);
            assert_eq!(
                (machine.metrics.entered_calls, machine.metrics.peak_calls),
                (calls, calls)
            );
            assert_eq!(machine.metrics.entered_finally_frames, finalies);
            assert_eq!(machine.metrics.retired_finally_frames, finalies);
            assert_eq!(
                (
                    machine.metrics.peak_error_leaf_capacity,
                    machine.metrics.peak_error_leaf_bytes
                ),
                (actual.count, actual.bytes)
            );
            assert_eq!(machine.metrics.peak_result_wrapper_capacity, 1);
            assert_eq!(machine.metrics.peak_string_payload_bytes, buffer.capacity);
            assert!(args.is_empty() && args.capacity() == 0);
            errors_drained(&machine);
            assert_eq!(host.call_depth, 0);
        }
    }
    for preentry in [false, true] {
        let (value, buffer) = string_input(PAYLOAD, 4096);
        let mut args = vec![value, Value::Int(if preentry { 0 } else { 512 })];
        let mut limits = Limits::default();
        if preentry {
            limits.values.max_storage_bytes = buffer.capacity - 1;
        }
        let mut machine = machine(RELAY, "Entry", limits);
        let mut host = production_interpreter(RELAY);
        let error = machine
            .run_owned_with_hooks("Entry", &mut args, 0, false, &mut host)
            .err()
            .expect("the real excess call or input budget must reject");
        if preentry {
            assert_eq!((host.steps, machine.metrics.entered_calls), (0, 0));
            let Value::String(text) = &args[0] else {
                unreachable!()
            };
            assert_eq!(
                (text.as_ptr() as usize, text.capacity()),
                (buffer.pointer, buffer.capacity)
            );
        } else {
            assert_eq!(error.code.as_deref(), Some("call_depth_exceeded"));
            assert_eq!(machine.metrics.entered_calls, 512);
            assert!(args.is_empty());
        }
        assert_eq!(
            (
                machine.metrics.peak_result_wrapper_capacity,
                machine.metrics.peak_error_leaf_capacity
            ),
            (0, 0)
        );
        errors_drained(&machine);
    }
}

#[test]
fn terminal_err_preallocation_and_actual_candidate_budgets_preserve_saved_primary() {
    const SOURCE: &str = "fn Entry(first: str, second: str, fallback: str): str! { try { return err(first) } finally { return err(second) }\nreturn ok(fallback) }";
    for actual_candidate in [false, true] {
        for count_rejection in [false, true] {
            let mut machine = machine(SOURCE, "Entry", Limits::default());
            let mut host = production_interpreter(SOURCE);
            let (first, a) = string_input("中\0primary", 1024);
            let (second, b) = string_input(PAYLOAD, 4096);
            let (fallback, c) = string_input("\0fallback", 512);
            let buffers = [a, b, c];
            let mut boxes = [None; 3];
            let mut errors = Vec::new();
            let mut args = vec![first, second, fallback];
            machine
                .start_owned_once("Entry", &mut args, 0, false)
                .unwrap();
            pause_at(&mut machine, &mut host, |m| {
                at_error_wrap(m) && m.result_wrapper_capacity == 1
            });
            observe_errors(&machine, &buffers, &mut boxes, &mut errors);
            let primary = errors[0];
            let Value::String(operand) = machine.values.get(machine.values.len() - 1).unwrap()
            else {
                unreachable!()
            };
            assert_eq!(
                (operand.as_ptr() as usize, operand.capacity()),
                (b.pointer, b.capacity)
            );
            if actual_candidate {
                // Real stdlib Map, without resizing or a fabricated allocator.
                // The budget is tightened after its construction to exercise
                // the private actual-admission helper; this does not claim the
                // source constructor's actual capacity exceeds its plan.
                let object = errors::error_object("ku", "err", operand.clone());
                let Value::Object(fields) = &object else {
                    unreachable!()
                };
                let Some(Value::String(message)) = fields.get("message") else {
                    unreachable!()
                };
                assert_ne!(
                    message.as_ptr() as usize,
                    operand.as_ptr() as usize,
                    "the original operand and real message.clone buffer are simultaneously alive"
                );
                assert_eq!(message, PAYLOAD);
                let (count, bytes) = error_backing(&object);
                let actual = AwaitPayloadCharge::error_object(&object).unwrap();
                assert_eq!(
                    (actual.error_leaf_count, actual.error_leaf_bytes),
                    (count, bytes)
                );
                let previous = AwaitPayloadCharge {
                    box_count: machine.result_wrapper_capacity,
                    box_bytes: machine.result_wrapper_bytes,
                    error_leaf_count: machine.error_leaf_capacity,
                    error_leaf_bytes: machine.error_leaf_bytes,
                    ..AwaitPayloadCharge::default()
                };
                machine.reserve_values(1, Span::default()).unwrap();
                let count_base = live_value_count(&machine);
                let byte_base = live_value_bytes(&machine);
                let saved = machine.value_limits;
                machine.value_limits.max_frames = count_base + actual.count;
                machine.value_limits.max_storage_bytes = byte_base + actual.bytes;
                machine
                    .reserve_error_candidate(previous, actual, Span::default())
                    .unwrap();
                machine.restore_error_candidate(previous);
                machine.value_limits = saved;
                if count_rejection {
                    machine.value_limits.max_frames = count_base + actual.count - 1;
                } else {
                    machine.value_limits.max_storage_bytes = byte_base + actual.bytes - 1;
                }
                let error = machine
                    .reserve_error_candidate(previous, actual, Span::default())
                    .unwrap_err();
                drop(object);
                machine.restore_error_candidate(previous);
                machine.pending = Some(Err(error));
            } else if count_rejection {
                machine.value_limits.max_frames = live_value_count(&machine);
            } else {
                machine.value_limits.max_storage_bytes = live_value_bytes(&machine);
            }
            if !actual_candidate {
                machine
                    .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut host)
                    .unwrap();
            }
            assert!(
                matches!(&machine.pending, Some(Err(error)) if error.message.contains(if count_rejection { "FrameLimit" } else { "StorageLimit" }))
            );
            assert_eq!(
                observe_errors(&machine, &buffers, &mut boxes, &mut errors),
                1
            );
            assert_eq!(errors[0], primary);
            assert!(
                boxes[1].is_none(),
                "the rejected candidate has no allocated Box"
            );
            finish_error(
                &mut machine,
                &mut host,
                1,
                &buffers,
                &mut boxes,
                &mut errors,
            )
            .unwrap_err();
            assert_eq!(machine.metrics.peak_result_wrapper_capacity, 1);
            errors_drained(&machine);
        }
    }
}

#[test]
fn terminal_err_q2_q3_keep_distinct_real_maps_until_arbitration_and_native_drop() {
    const Q2: &str = "fn Entry(first: str, second: str, third: str, fallback: str): str! { try { return err(first) } finally { return err(second) }\nreturn ok(fallback) }";
    const Q3: &str = "fn Entry(first: str, second: str, third: str, fallback: str): str! { try { return err(first) } finally { try { return err(second) } finally { return err(third) } }\nreturn ok(fallback) }";
    for (source, owners) in [(Q2, 2), (Q3, 3)] {
        for native in [false, true] {
            for chunk in [1, 7, 256] {
                let (first, a) = string_input("中\0primary", 1024);
                let (second, b) = string_input("é\0replacement", 4096);
                let (third, c) = string_input(PAYLOAD, 2048);
                let (fallback, d) = string_input("\0fallback", 512);
                let buffers = [a, b, c, d];
                let mut boxes = [None; 4];
                let mut payloads = Vec::new();
                let mut machine = machine(source, "Entry", Limits::default());
                let mut host = production_interpreter(source);
                let mut args = vec![first, second, third, fallback];
                machine
                    .start_owned_once("Entry", &mut args, 0, false)
                    .unwrap();
                pause_at(&mut machine, &mut host, |m| {
                    m.result_wrapper_capacity == owners
                });
                assert_eq!(
                    observe_errors(&machine, &buffers, &mut boxes, &mut payloads),
                    owners
                );
                assert_eq!(machine.finally.frames.len(), owners - 1);
                assert_eq!(
                    machine.error_leaf_capacity,
                    payloads.iter().map(|p| p.count).sum::<usize>()
                );
                assert_eq!(
                    machine.error_leaf_bytes,
                    payloads.iter().map(|p| p.bytes).sum::<usize>()
                );
                if native {
                    let native =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                            let _cleanup = CleanupGuard::enter(CancellationContext::new(
                                TerminationReason::Cancelled,
                            ));
                            let _abandoned = machine;
                            panic!("actual terminal err Map kernel abandonment");
                        }));
                    assert_eq!(
                        native.unwrap_err().downcast_ref::<&str>().copied(),
                        Some("actual terminal err Map kernel abandonment")
                    );
                    assert!(current_cleanup_context().is_none());
                    assert!(crate::runtime::task::current_execution_termination().is_none());
                } else {
                    let result = finish_error(
                        &mut machine,
                        &mut host,
                        chunk,
                        &buffers,
                        &mut boxes,
                        &mut payloads,
                    )
                    .unwrap();
                    assert_eq!(outcome_buffer(&result, &buffers).0, owners - 1);
                    assert_eq!(machine.metrics.peak_result_wrapper_capacity, owners);
                    assert_eq!(
                        (
                            machine.metrics.peak_error_leaf_capacity,
                            machine.metrics.peak_error_leaf_bytes
                        ),
                        (
                            payloads.iter().map(|payload| payload.count).sum::<usize>(),
                            payloads.iter().map(|payload| payload.bytes).sum::<usize>()
                        )
                    );
                    assert_eq!(machine.metrics.entered_finally_frames, owners - 1);
                    errors_drained(&machine);
                }
                let mut healthy =
                    production_interpreter("fn Healthy(text: str): str! { return err(text) }");
                let (text, buffer) = string_input(PAYLOAD, 4096);
                let result = healthy
                    .call_function("Healthy", vec![text], Span::default(), 0)
                    .unwrap();
                outcome_buffer(&result, &[buffer]);
                assert_eq!(healthy.call_depth, 0);
            }
        }
    }
}

use code::PrimitiveResultKind;

const PRIMITIVE_CASES: [(PrimitiveResultKind, &str, &str); 4] = [
    (PrimitiveResultKind::Int, "int", "7"),
    (PrimitiveResultKind::Float, "float", "1.25"),
    (PrimitiveResultKind::Bool, "bool", "true"),
    (PrimitiveResultKind::Null, "null", "null"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrimitiveLeaf {
    Int(i64),
    Float(u64),
    Bool(bool),
    Null,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PrimitiveBox {
    address: usize,
    leaf: PrimitiveLeaf,
    error: Option<ErrorPayload>,
}

fn expected_primitive(kind: PrimitiveResultKind) -> PrimitiveLeaf {
    match kind {
        PrimitiveResultKind::Int => PrimitiveLeaf::Int(7),
        PrimitiveResultKind::Float => PrimitiveLeaf::Float(1.25_f64.to_bits()),
        PrimitiveResultKind::Bool => PrimitiveLeaf::Bool(true),
        PrimitiveResultKind::Null => PrimitiveLeaf::Null,
    }
}

fn primitive_snapshot(
    value: &Value,
    kind: PrimitiveResultKind,
    buffers: &[Buffer],
) -> Option<PrimitiveBox> {
    let Value::Result { ok, value: leaf } = value else {
        return None;
    };
    let address = leaf.as_ref() as *const Value as usize;
    let (leaf, error) = if *ok {
        let actual = match leaf.as_ref() {
            Value::Int(value) if kind == PrimitiveResultKind::Int => PrimitiveLeaf::Int(*value),
            Value::Float(value) if kind == PrimitiveResultKind::Float => PrimitiveLeaf::Float(value.to_bits()),
            Value::Bool(value) if kind == PrimitiveResultKind::Bool => PrimitiveLeaf::Bool(*value),
            Value::Null if kind == PrimitiveResultKind::Null => PrimitiveLeaf::Null,
            value => panic!("this fixture's actual primitive payload must match its expected variant: {kind:?}/{value:?}"),
        };
        (actual, None)
    } else {
        let actual =
            error_payload(value).expect("a selected primitive err retains the real ErrorObject");
        let Value::Object(fields) = leaf.as_ref() else {
            unreachable!()
        };
        assert!(matches!(fields.get("domain"), Some(Value::String(domain)) if domain == "ku"));
        assert!(matches!(fields.get("code"), Some(Value::String(code)) if code == "err"));
        let Some(Value::String(message)) = fields.get("message") else {
            unreachable!()
        };
        let input = buffers
            .iter()
            .find(|buffer| buffer.text == message.as_str())
            .expect("the error message comes from one real input");
        if !message.is_empty() {
            assert_ne!(
                message.as_ptr() as usize,
                input.pointer,
                "primitive err retains the existing message.clone contract"
            );
        }
        assert_eq!(actual.boxed, address);
        (PrimitiveLeaf::Error, Some(actual))
    };
    Some(PrimitiveBox {
        address,
        leaf,
        error,
    })
}

fn observe_primitive(
    machine: &Machine,
    kind: PrimitiveResultKind,
    buffers: &[Buffer],
    seen: &mut Vec<PrimitiveBox>,
) -> usize {
    let mut live = Vec::new();
    let mut observe = |value: &Value| {
        if let Some(actual) = primitive_snapshot(value, kind, buffers) {
            assert!(
                live.iter()
                    .all(|owner: &PrimitiveBox| owner.address != actual.address),
                "one actual owning location per primitive Box"
            );
            if let Some(previous) = seen.iter().find(|owner| owner.address == actual.address) {
                assert_eq!(
                    *previous, actual,
                    "relays and saved primaries move the same real Box and ErrorObject backing"
                );
            } else {
                seen.push(actual);
            }
            live.push(actual);
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
    for frame in machine.finally.frames.as_slice() {
        if let Some(Ok(Flow::Return { value, .. } | Flow::Fail { value, .. })) = &frame.primary {
            observe(value);
        }
    }
    assert!(
        machine.awaiting.is_none(),
        "S5 remains a synchronous source tree"
    );
    let count = live.len();
    let bytes = count * std::mem::size_of::<Value>();
    let error_count = live
        .iter()
        .filter_map(|owner| owner.error)
        .map(|error| error.count)
        .sum::<usize>();
    let error_bytes = live
        .iter()
        .filter_map(|owner| owner.error)
        .map(|error| error.bytes)
        .sum::<usize>();
    assert_eq!(
        (
            machine.result_wrapper_capacity,
            machine.result_wrapper_bytes
        ),
        (count, bytes)
    );
    assert_eq!(
        (
            machine.metrics.retained_result_wrapper_capacity,
            machine.metrics.retained_result_wrapper_bytes
        ),
        (count, bytes)
    );
    assert_eq!(
        (machine.error_leaf_capacity, machine.error_leaf_bytes),
        (error_count, error_bytes)
    );
    assert_eq!(
        (
            machine.metrics.retained_error_leaf_capacity,
            machine.metrics.retained_error_leaf_bytes
        ),
        (error_count, error_bytes)
    );
    count
}

fn finish_primitive(
    machine: &mut Machine,
    host: &mut Interpreter,
    chunk: usize,
    kind: PrimitiveResultKind,
    buffers: &[Buffer],
    seen: &mut Vec<PrimitiveBox>,
) -> KuResult<Value> {
    let pool = buffers.iter().map(|buffer| buffer.capacity).sum::<usize>();
    let budget = NonZeroUsize::new(chunk).unwrap();
    for _ in 0..100_000 {
        match machine.dispatch_chunk(budget, host).unwrap() {
            ChunkProgress::More { popped } => {
                assert_eq!(popped, chunk);
                assert_eq!(
                    machine.string_payload_bytes, pool,
                    "the original input capacity stays conservatively reserved for the whole run"
                );
                observe_primitive(machine, kind, buffers, seen);
            }
            ChunkProgress::Waiting { .. } => {
                panic!("a selected synchronous primitive tree cannot await")
            }
            ChunkProgress::Finished { popped, result } => {
                assert!(popped <= chunk);
                let result =
                    result.and_then(|flow| flow.into_call_value(Span::default(), "function"));
                if let Ok(value) = &result {
                    let actual = primitive_snapshot(value, kind, buffers)
                        .expect("the actual outgoing typed primitive Result");
                    let previous = seen.iter().find(|owner| owner.address == actual.address);
                    if chunk == 1 {
                        assert_eq!(
                            *previous.expect(
                                "one-step dispatch must independently observe the outgoing Box"
                            ),
                            actual
                        );
                    } else if let Some(previous) = previous {
                        assert_eq!(*previous, actual);
                    }
                }
                return result;
            }
        }
    }
    panic!("the finite primitive Result source did not finish");
}

fn at_primitive_wrap(machine: &Machine) -> bool {
    machine.pending.is_none()
        && machine.exits.len() == 0
        && matches!(
            machine
                .work
                .len()
                .checked_sub(1)
                .and_then(|index| machine.work.get(index)),
            Some(Work::WrapOwnedScalarResult(_, _))
        )
}

fn primitive_budget_entries(machine: &mut Machine, rejection: Option<&str>) {
    let span = Span::default();
    let spare = machine.values.capacity() - machine.values.len();
    let results = [
        machine.reserve_values(spare, span),
        machine.check_argument_capacity(0, span),
        machine.check_exit_storage_capacity(0, 0, span),
        machine.check_lease_capacity(0, 0, span),
    ];
    for result in results {
        if let Some(reason) = rejection {
            assert!(result.unwrap_err().message.contains(reason));
        } else {
            result.unwrap();
        }
    }
}

fn check_primitive_live_budget(machine: &mut Machine) {
    let saved = machine.value_limits;
    let count = live_value_count(machine);
    let bytes = live_value_bytes(machine);
    machine.value_limits.max_frames = count;
    machine.value_limits.max_storage_bytes = bytes;
    primitive_budget_entries(machine, None);
    machine.value_limits.max_frames = count - 1;
    primitive_budget_entries(machine, Some("FrameLimit"));
    machine.value_limits.max_frames = count;
    machine.value_limits.max_storage_bytes = bytes - 1;
    primitive_budget_entries(machine, Some("StorageLimit"));
    machine.value_limits = saved;
}

#[test]
fn owned_primitive_four_kinds_helpers_and_same_kind_relays_keep_512_and_first_513() {
    for (kind, ty, literal) in PRIMITIVE_CASES {
        let direct = format!("fn Entry(text: str, n: int): {ty}! {{ return ok({literal}) }}");
        let plain_operand = match kind {
            PrimitiveResultKind::Int => "text.len()",
            PrimitiveResultKind::Bool => "text.len() > 0",
            _ => literal,
        };
        let helper = format!("fn Produce(text: str): {ty} {{ return {plain_operand} }}\nfn Entry(text: str, n: int): {ty}! {{ return ok(Produce(text)) }}");
        let typed_operand = match kind {
            PrimitiveResultKind::Int => "text.len()",
            PrimitiveResultKind::Bool => "text.byte_len() > 0",
            _ => literal,
        };
        let typed_helper = format!("fn Leaf(text: str): {ty}! {{ return ok({typed_operand}) }}\nfn Entry(text: str, n: int): {ty}! {{ return Leaf(text) }}");
        let error_helper = format!("fn Keep(text: str): str {{ try {{ marker = 1 }} finally {{ marker = 2 }}\nreturn text }}\nfn Entry(text: str, n: int): {ty}! {{ return err(Keep(text)) }}");
        let read_expected = match kind {
            PrimitiveResultKind::Int => PrimitiveLeaf::Int(PAYLOAD.chars().count() as i64),
            PrimitiveResultKind::Bool => PrimitiveLeaf::Bool(true),
            _ => expected_primitive(kind),
        };
        let byte_helper = "fn ByteLength(text: str): int { return text.byte_len() }\nfn Entry(text: str, n: int): int! { return ok(ByteLength(text)) }".to_owned();
        let byte_result_helper = "fn ByteLength(text: str): int! { return ok(text.byte_len()) }\nfn Entry(text: str, n: int): int! { return ByteLength(text) }".to_owned();
        let mut shallow_sources = vec![
            (&direct, 1, 0, expected_primitive(kind)),
            (&helper, 2, 0, read_expected),
            (&typed_helper, 2, 0, read_expected),
            (&error_helper, 2, 1, PrimitiveLeaf::Error),
        ];
        if kind == PrimitiveResultKind::Int {
            let byte_expected = PrimitiveLeaf::Int(PAYLOAD.len() as i64);
            shallow_sources.push((&byte_helper, 2, 0, byte_expected));
            shallow_sources.push((&byte_result_helper, 2, 0, byte_expected));
        }
        for (source, calls, finalies, expected) in shallow_sources {
            for chunk in [1, 7, 256] {
                let mut machine = machine(source, "Entry", Limits::default());
                let mut host = production_interpreter(source);
                let (text, buffer) = string_input(PAYLOAD, 4096);
                let mut args = vec![text, Value::Int(0)];
                let mut seen = Vec::new();
                machine
                    .start_owned_once("Entry", &mut args, 0, false)
                    .unwrap();
                let result =
                    finish_primitive(&mut machine, &mut host, chunk, kind, &[buffer], &mut seen)
                        .unwrap();
                let actual = primitive_snapshot(&result, kind, &[buffer]).unwrap();
                assert_eq!(actual.leaf, expected);
                assert_eq!(
                    (
                        machine.metrics.entered_calls,
                        machine.metrics.peak_calls,
                        machine.metrics.entered_finally_frames,
                        machine.metrics.retired_finally_frames
                    ),
                    (calls, calls, finalies, finalies)
                );
                assert_eq!(machine.metrics.peak_result_wrapper_capacity, 1);
                assert!(args.is_empty() && args.capacity() == 0);
                errors_drained(&machine);
                assert_eq!(host.call_depth, 0);
            }
        }
        let mut leaves = vec![(format!("ok({literal})"), expected_primitive(kind))];
        if kind == PrimitiveResultKind::Int {
            leaves.push((
                "ok(text.len())".to_owned(),
                PrimitiveLeaf::Int(PAYLOAD.chars().count() as i64),
            ));
            leaves.push((
                "ok(text.byte_len())".to_owned(),
                PrimitiveLeaf::Int(PAYLOAD.len() as i64),
            ));
        } else if kind == PrimitiveResultKind::Bool {
            leaves.push(("ok(text.len() > 0)".to_owned(), PrimitiveLeaf::Bool(true)));
            leaves.push((
                "ok(text.byte_len() > 0)".to_owned(),
                PrimitiveLeaf::Bool(true),
            ));
        }
        leaves.push(("err(text)".to_owned(), PrimitiveLeaf::Error));
        for (leaf, expected) in leaves {
            let relay = format!("fn Entry(text: str, n: int): {ty}! {{ if (n <= 0) {{ return {leaf} }}\nreturn Entry(text, n - 1) }}");
            let mutual = format!("fn Other(text: str, n: int): {ty}! {{ if (n <= 0) {{ return {leaf} }}\nreturn Entry(text, n - 1) }}\nfn Entry(text: str, n: int): {ty}! {{ if (n <= 0) {{ return {leaf} }}\nreturn Other(text, n - 1) }}");
            for source in [&relay, &mutual] {
                for chunk in [1, 7, 256] {
                    for n in [511, 512] {
                        let mut machine = machine(source, "Entry", Limits::default());
                        let mut host = production_interpreter(source);
                        let (text, buffer) = string_input(PAYLOAD, 4096);
                        let mut args = vec![text, Value::Int(n)];
                        let mut seen = Vec::new();
                        machine
                            .start_owned_once("Entry", &mut args, 0, false)
                            .unwrap();
                        let result = finish_primitive(
                            &mut machine,
                            &mut host,
                            chunk,
                            kind,
                            &[buffer],
                            &mut seen,
                        );
                        assert_eq!(
                            (machine.metrics.entered_calls, machine.metrics.peak_calls),
                            (512, 512)
                        );
                        if n == 511 {
                            let actual =
                                primitive_snapshot(&result.unwrap(), kind, &[buffer]).unwrap();
                            assert_eq!(actual.leaf, expected);
                            assert_eq!(machine.metrics.peak_result_wrapper_capacity, 1);
                            if chunk == 1 {
                                assert_eq!(
                                    seen.len(),
                                    1,
                                    "every real relay boundary was physically observed"
                                );
                            }
                        } else {
                            assert_eq!(
                                result.unwrap_err().code.as_deref(),
                                Some("call_depth_exceeded")
                            );
                            assert_eq!(
                                (
                                    machine.metrics.peak_result_wrapper_capacity,
                                    machine.metrics.peak_error_leaf_capacity
                                ),
                                (0, 0)
                            );
                        }
                        assert_eq!(machine.metrics.peak_string_payload_bytes, buffer.capacity);
                        errors_drained(&machine);
                        assert_eq!(host.call_depth, 0);
                    }
                }
            }
        }
    }
}

#[test]
fn owned_primitive_input_pool_and_four_budget_entries_reject_before_real_box_and_reuse() {
    for (kind, ty, literal) in PRIMITIVE_CASES {
        let source = format!("fn Entry(text: str): {ty}! {{ return ok({literal}) }}");
        let (text, buffer) = string_input(PAYLOAD, 4096);
        let mut args = vec![text];
        let mut limits = Limits::default();
        limits.values.max_storage_bytes = buffer.capacity - 1;
        let mut rejected = machine(&source, "Entry", limits);
        let mut host = production_interpreter(&source);
        let error = rejected
            .run_owned_with_hooks("Entry", &mut args, 0, false, &mut host)
            .err()
            .expect("the original input pool must be admitted before body entry");
        assert!(error.message.contains("StorageLimit"));
        assert_eq!((host.steps, rejected.metrics.entered_calls), (0, 0));
        let Value::String(text) = &args[0] else {
            unreachable!()
        };
        assert_eq!(
            (text.as_ptr() as usize, text.capacity()),
            (buffer.pointer, buffer.capacity)
        );
        errors_drained(&rejected);
        for boundary in [None, Some("FrameLimit"), Some("StorageLimit")] {
            let mut machine = machine(&source, "Entry", Limits::default());
            let mut host = production_interpreter(&source);
            let (text, buffer) = string_input(PAYLOAD, 4096);
            let mut args = vec![text];
            let mut seen = Vec::new();
            machine
                .start_owned_once("Entry", &mut args, 0, false)
                .unwrap();
            pause_at(&mut machine, &mut host, at_primitive_wrap);
            assert_eq!(machine.string_payload_bytes, buffer.capacity);
            assert_eq!(machine.result_wrapper_capacity, 0);
            check_primitive_live_budget(&mut machine);
            machine.reserve_values(1, Span::default()).unwrap();
            let count = live_value_count(&machine);
            let bytes = live_value_bytes(&machine);
            machine.value_limits.max_frames = count + usize::from(boundary != Some("FrameLimit"));
            machine.value_limits.max_storage_bytes = bytes + std::mem::size_of::<Value>()
                - usize::from(boundary == Some("StorageLimit"));
            machine
                .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut host)
                .unwrap();
            if let Some(reason) = boundary {
                assert!(
                    matches!(&machine.pending, Some(Err(error)) if error.message.contains(reason))
                );
                assert_eq!(observe_primitive(&machine, kind, &[buffer], &mut seen), 0);
                assert_eq!(
                    machine.metrics.peak_result_wrapper_capacity, 0,
                    "the failed candidate never allocates its Box"
                );
            } else {
                assert_eq!(observe_primitive(&machine, kind, &[buffer], &mut seen), 1);
                assert_eq!(seen[0].leaf, expected_primitive(kind));
                check_primitive_live_budget(&mut machine);
            }
            machine.value_limits = Limits::default().values;
            let result = finish_primitive(&mut machine, &mut host, 1, kind, &[buffer], &mut seen);
            assert_eq!(result.is_err(), boundary.is_some());
            errors_drained(&machine);
            machine.prepare_next_run().unwrap();
            let (text, buffer) = string_input(PAYLOAD, 4096);
            let mut args = vec![text];
            seen.clear();
            machine
                .start_owned_once("Entry", &mut args, 0, false)
                .unwrap();
            let result =
                finish_primitive(&mut machine, &mut host, 7, kind, &[buffer], &mut seen).unwrap();
            assert_eq!(
                primitive_snapshot(&result, kind, &[buffer]).unwrap().leaf,
                expected_primitive(kind)
            );
            errors_drained(&machine);
        }
    }
}

#[test]
fn owned_primitive_err_charges_real_box_map_and_six_cloned_buffers_beyond_input_pool() {
    for (kind, ty, _) in PRIMITIVE_CASES {
        let source = format!("fn Keep(text: str): str {{ try {{ marker = 1 }} finally {{ marker = 2 }}\nreturn text }}\nfn Entry(text: str): {ty}! {{ return err(Keep(text)) }}");
        for chunk in [1, 7, 256] {
            let mut machine = machine(&source, "Entry", Limits::default());
            let mut host = production_interpreter(&source);
            let (text, buffer) = string_input(PAYLOAD, 4096);
            let mut args = vec![text];
            let mut seen = Vec::new();
            machine
                .start_owned_once("Entry", &mut args, 0, false)
                .unwrap();
            pause_at(&mut machine, &mut host, at_error_wrap);
            let Value::String(original) = machine.values.get(machine.values.len() - 1).unwrap()
            else {
                unreachable!()
            };
            assert_eq!(
                (original.as_ptr() as usize, original.capacity()),
                (buffer.pointer, buffer.capacity)
            );
            assert_eq!(
                (machine.result_wrapper_capacity, machine.error_leaf_capacity),
                (0, 0)
            );
            machine
                .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut host)
                .unwrap();
            assert_eq!(observe_primitive(&machine, kind, &[buffer], &mut seen), 1);
            let actual = seen[0].error.unwrap();
            assert_eq!(
                (machine.error_leaf_capacity, machine.error_leaf_bytes),
                (actual.count, actual.bytes)
            );
            assert_eq!(machine.string_payload_bytes, buffer.capacity);
            check_primitive_live_budget(&mut machine);
            let result =
                finish_primitive(&mut machine, &mut host, chunk, kind, &[buffer], &mut seen)
                    .unwrap();
            assert_eq!(
                primitive_snapshot(&result, kind, &[buffer]).unwrap().error,
                Some(actual)
            );
            assert_eq!(
                (
                    machine.metrics.peak_result_wrapper_capacity,
                    machine.metrics.peak_error_leaf_capacity,
                    machine.metrics.peak_error_leaf_bytes
                ),
                (1, actual.count, actual.bytes)
            );
            assert_eq!(
                (
                    machine.metrics.entered_calls,
                    machine.metrics.entered_finally_frames,
                    machine.metrics.retired_finally_frames
                ),
                (2, 1, 1)
            );
            errors_drained(&machine);
        }
    }
}

#[test]
fn owned_primitive_q2_q3_arbitrate_real_saved_boxes_with_chunk_one_owner_evidence() {
    for (kind, ty, literal) in PRIMITIVE_CASES {
        for (errors, owners) in [
            ([false, false, false], 2),
            ([true, true, false], 2),
            ([true, false, false], 2),
            ([false, true, false], 2),
            ([true, false, true], 3),
        ] {
            let outcome = |error: bool, input: &str| {
                if error {
                    format!("err({input})")
                } else {
                    format!("ok({literal})")
                }
            };
            let first = outcome(errors[0], "first");
            let second = outcome(errors[1], "second");
            let third = outcome(errors[2], "third");
            let finally = if owners == 2 {
                format!("return {second}")
            } else {
                format!("try {{ return {second} }} finally {{ return {third} }}")
            };
            let source = format!("fn Entry(first: str, second: str, third: str): {ty}! {{ try {{ return {first} }} finally {{ {finally} }}\nreturn ok({literal}) }}");
            for chunk in [1, 7, 256] {
                let mut machine = machine(&source, "Entry", Limits::default());
                let mut host = production_interpreter(&source);
                let (first, a) = string_input("中\0primary", 1024);
                let (second, b) = string_input("é\0replacement", 4096);
                let (third, c) = string_input(PAYLOAD, 2048);
                let buffers = [a, b, c];
                let mut args = vec![first, second, third];
                let mut seen = Vec::new();
                machine
                    .start_owned_once("Entry", &mut args, 0, false)
                    .unwrap();
                let mut finally_candidate = None;
                if chunk == 1 {
                    // Only this actual one-step prefix claims physical
                    // intermediate owner evidence. Other chunks run intact.
                    pause_at(&mut machine, &mut host, |m| {
                        m.result_wrapper_capacity == owners
                    });
                    assert_eq!(
                        observe_primitive(&machine, kind, &buffers, &mut seen),
                        owners
                    );
                    let candidate = primitive_snapshot(
                        machine.values.as_slice().last().expect(
                            "the newly wrapped finally candidate is on the real value stack",
                        ),
                        kind,
                        &buffers,
                    )
                    .expect("the final finally candidate is a real Result Box");
                    assert!(seen.contains(&candidate));
                    finally_candidate = Some(candidate);
                    assert_eq!(machine.finally.frames.len(), owners - 1);
                    check_primitive_live_budget(&mut machine);
                    let physical_errors = seen
                        .iter()
                        .filter_map(|owner| owner.error)
                        .collect::<Vec<_>>();
                    assert_eq!(
                        (machine.error_leaf_capacity, machine.error_leaf_bytes),
                        (
                            physical_errors
                                .iter()
                                .map(|error| error.count)
                                .sum::<usize>(),
                            physical_errors
                                .iter()
                                .map(|error| error.bytes)
                                .sum::<usize>()
                        )
                    );
                }
                let result =
                    finish_primitive(&mut machine, &mut host, chunk, kind, &buffers, &mut seen)
                        .unwrap();
                let actual = primitive_snapshot(&result, kind, &buffers).unwrap();
                if let Some(candidate) = finally_candidate {
                    assert_eq!(
                        actual.address, candidate.address,
                        "the last actual finally candidate Box wins arbitration"
                    );
                    assert_eq!(
                        actual, candidate,
                        "the winning Box keeps its independently observed payload backing"
                    );
                }
                if errors[owners - 1] {
                    assert_eq!(outcome_buffer(&result, &buffers).0, owners - 1);
                }
                assert_eq!(
                    actual.leaf,
                    if errors[owners - 1] {
                        PrimitiveLeaf::Error
                    } else {
                        expected_primitive(kind)
                    }
                );
                assert_eq!(
                    (
                        machine.metrics.peak_result_wrapper_capacity,
                        machine.metrics.peak_result_wrapper_bytes
                    ),
                    (owners, owners * std::mem::size_of::<Value>())
                );
                assert_eq!(
                    (
                        machine.metrics.entered_finally_frames,
                        machine.metrics.retired_finally_frames
                    ),
                    (owners - 1, owners - 1)
                );
                if chunk == 1 {
                    assert_eq!(seen.len(), owners);
                    assert_eq!(
                        (
                            machine.metrics.peak_error_leaf_capacity,
                            machine.metrics.peak_error_leaf_bytes
                        ),
                        (
                            seen.iter()
                                .filter_map(|owner| owner.error)
                                .map(|error| error.count)
                                .sum::<usize>(),
                            seen.iter()
                                .filter_map(|owner| owner.error)
                                .map(|error| error.bytes)
                                .sum::<usize>()
                        )
                    );
                }
                assert_eq!(
                    machine.metrics.peak_string_payload_bytes,
                    buffers.iter().map(|buffer| buffer.capacity).sum::<usize>()
                );
                errors_drained(&machine);
            }
        }
    }
}

#[test]
fn owned_primitive_later_argument_fuel_and_actual_native_unwind_restore_tls_and_health() {
    for (kind, ty, literal) in PRIMITIVE_CASES {
        let source = format!("fn Entry(text: str): {ty}! {{ return ok({literal}) }}");
        let failure = format!("fn Leaf(text: str, ignored: int): {ty}! {{ return ok({literal}) }}\nfn Entry(text: str): {ty}! {{ return Leaf(text, 1 / 0) }}");
        let mut failed_machine = machine(&failure, "Entry", Limits::default());
        let mut host = production_interpreter(&failure);
        let (text, buffer) = string_input(PAYLOAD, 4096);
        let mut args = vec![text];
        let mut seen = Vec::new();
        failed_machine
            .start_owned_once("Entry", &mut args, 0, false)
            .unwrap();
        let error = finish_primitive(
            &mut failed_machine,
            &mut host,
            1,
            kind,
            &[buffer],
            &mut seen,
        )
        .unwrap_err();
        assert_eq!(error.message, "division by zero");
        assert_eq!(
            (
                failed_machine.metrics.entered_calls,
                failed_machine.metrics.peak_result_wrapper_capacity,
                failed_machine.metrics.peak_error_leaf_capacity
            ),
            (1, 0, 0)
        );
        errors_drained(&failed_machine);
        for after_wrap in [false, true] {
            let mut machine = machine(&source, "Entry", Limits::default());
            let mut host = production_interpreter(&source);
            let (text, buffer) = string_input(PAYLOAD, 4096);
            let mut args = vec![text];
            let mut seen = Vec::new();
            machine
                .start_owned_once("Entry", &mut args, 0, true)
                .unwrap();
            pause_at(&mut machine, &mut host, at_primitive_wrap);
            if after_wrap {
                machine
                    .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut host)
                    .unwrap();
                assert_eq!(observe_primitive(&machine, kind, &[buffer], &mut seen), 1);
            }
            machine.remaining_dispatches = 0;
            let error = finish_primitive(&mut machine, &mut host, 1, kind, &[buffer], &mut seen)
                .unwrap_err();
            assert_eq!(
                error.message,
                "internal execution prototype test fuel exhausted"
            );
            assert_eq!(
                machine.metrics.peak_result_wrapper_capacity,
                usize::from(after_wrap)
            );
            errors_drained(&machine);
        }
        for (error, after_wrap) in [(false, false), (false, true), (true, true)] {
            let source = if error {
                format!("fn Entry(text: str): {ty}! {{ return err(text) }}")
            } else {
                source.clone()
            };
            let now = Instant::now();
            let original = RuntimeTermination {
                reason: TerminationReason::TimedOut,
                cleanup_deadline: now,
            };
            let shorter = RuntimeTermination {
                reason: TerminationReason::Cancelled,
                cleanup_deadline: now.checked_sub(Duration::from_millis(1)).unwrap(),
            };
            let native = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _cleanup = CleanupGuard::enter(original);
                let mut machine = machine(&source, "Entry", Limits::default());
                let mut host = production_interpreter(&source);
                let (text, buffer) = string_input(PAYLOAD, 4096);
                let mut args = vec![text];
                let mut seen = Vec::new();
                machine
                    .start_owned_once("Entry", &mut args, 0, false)
                    .unwrap();
                pause_at(
                    &mut machine,
                    &mut host,
                    if error {
                        at_error_wrap
                    } else {
                        at_primitive_wrap
                    },
                );
                if after_wrap {
                    machine
                        .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut host)
                        .unwrap();
                }
                assert_eq!(
                    observe_primitive(&machine, kind, &[buffer], &mut seen),
                    usize::from(after_wrap)
                );
                drop(CleanupGuard::enter(shorter));
                let actual = current_cleanup_context().unwrap();
                assert_eq!(
                    (actual.reason, actual.cleanup_deadline),
                    (original.reason, shorter.cleanup_deadline)
                );
                panic!("actual owned primitive Result kernel abandonment");
            }));
            assert_eq!(
                native
                    .expect_err("the real native panic must escape")
                    .downcast_ref::<&str>()
                    .copied(),
                Some("actual owned primitive Result kernel abandonment")
            );
            assert!(current_cleanup_context().is_none());
            assert!(crate::runtime::task::current_execution_termination().is_none());
            let healthy_source = format!("fn Entry(text: str): {ty}! {{ return ok({literal}) }}");
            let mut healthy = production_interpreter(&healthy_source);
            let (text, buffer) = string_input(PAYLOAD, 4096);
            let result = healthy
                .call_function("Entry", vec![text], Span::default(), 0)
                .unwrap();
            assert_eq!(
                primitive_snapshot(&result, kind, &[buffer]).unwrap().leaf,
                expected_primitive(kind)
            );
            assert_eq!(healthy.call_depth, 0);
        }
    }
}

#[test]
fn owned_primitive_exact_arena_descriptors_and_legal_whole_tree_rejections_keep_fallback_inputs() {
    for (kind, ty, literal) in PRIMITIVE_CASES {
        let source = format!("fn Leaf(text: str): {ty}! {{ return ok({literal}) }}\nfn Entry(text: str): {ty}! {{ return Leaf(text) }}");
        let source_functions = functions(&source);
        let entry = &source_functions["Entry"];
        assert!(code::eligible_owned_named(
            &source_functions,
            entry,
            Limits::default()
        ));
        let code =
            CodeHandle::compile_owned_named(&source_functions, entry, Limits::default()).unwrap();
        for name in ["Entry", "Leaf"] {
            assert_eq!(
                code.function(code.find_function(name).unwrap())
                    .owned_scalar_result,
                Some(kind)
            );
        }
        // Both descriptors above belong to this single admitted arena. The
        // Builder's private cached-ID role checks are tested in code.rs.
        let (wrong_ty, wrong_literal) = if kind == PrimitiveResultKind::Float {
            ("int", "7")
        } else {
            ("float", "1.25")
        };
        let cross_kind_return = format!("fn Wrong(text: str): {wrong_ty}! {{ return ok({wrong_literal}) }}\nfn Entry(text: str): {ty}! {{ return ok({literal})\nreturn Wrong(text) }}\nfn main() {{}}");
        let program = Parser::new(Lexer::new(&cross_kind_return).tokenize().unwrap())
            .parse_program()
            .unwrap();
        let error = crate::checker::Checker::new()
            .check(&program)
            .expect_err("the checker rejects different-kind returns even after an earlier return");
        assert!(
            error
                .message
                .contains(&format!("type error: expected {ty}! but got {wrong_ty}!")),
            "{error:?}"
        );
        // This legal dead expression statement is outside the strict Result
        // slice. The private same-arena typed-call contract is tested in code.rs.
        let cross_kind = format!("fn Wrong(text: str): {wrong_ty}! {{ return ok({wrong_literal}) }}\nfn Entry(text: str): {ty}! {{ return ok({literal})\nWrong(text) }}");
        let stored =
            format!("fn Entry(text: str): {ty}! {{ result = ok({literal})\nreturn result }}");
        let nested = format!("fn Leaf(text: str): {ty}! {{ return ok({literal}) }}\nfn Entry(text: str): {ty}! {{ return ok(Leaf(text)?) }}");
        let caught = format!("fn Entry(text: str): {ty}! {{ try {{ marker = 1 }} catch (e) {{ marker = 2 }}\nreturn ok({literal}) }}");
        let async_entry = format!("async fn Entry(text: str): {ty}! {{ return ok({literal}) }}");
        let async_dependency = format!("async fn Child(text: str): {ty}! {{ return ok({literal}) }}\nfn Entry(text: str): {ty}! {{ task = Child(text)\nreturn ok({literal}) }}");
        for source in [
            &cross_kind,
            &stored,
            &nested,
            &caught,
            &async_entry,
            &async_dependency,
        ] {
            let source_functions = functions(source);
            let entry = &source_functions["Entry"];
            assert!(
                !code::eligible_owned_named(&source_functions, entry, Limits::default()),
                "{source}"
            );
            assert!(
                matches!(
                    CodeHandle::compile_owned_named(&source_functions, entry, Limits::default()),
                    Err(AdmissionError::Unsupported(_))
                ),
                "{source}"
            );
            let mut host = production_interpreter(source);
            let (text, buffer) = string_input(PAYLOAD, 4096);
            let mut args = vec![text];
            assert!(try_execute_owned(&mut host, entry, &mut args, 0).is_none());
            assert_eq!((host.steps, host.call_depth), (0, 0));
            let Value::String(text) = &args[0] else {
                unreachable!()
            };
            assert_eq!(
                (text.as_ptr() as usize, text.capacity()),
                (buffer.pointer, buffer.capacity)
            );
        }
        let mismatch = format!("fn Entry(text: str, spare: str): {ty}! {{ return ok({literal}) }}");
        let source_functions = functions(&mismatch);
        let mut host = production_interpreter(&mismatch);
        let (spare, buffer) = string_input(PAYLOAD, 4096);
        let mut args = vec![Value::Int(1), spare];
        assert!(try_execute_owned(&mut host, &source_functions["Entry"], &mut args, 0).is_none());
        assert_eq!((host.steps, host.call_depth), (0, 0));
        let Value::String(spare) = &args[1] else {
            unreachable!()
        };
        assert_eq!(
            (spare.as_ptr() as usize, spare.capacity()),
            (buffer.pointer, buffer.capacity)
        );
        let scalar_source = functions(&format!(
            "fn Entry(n: int): {ty}! {{ return ok({literal}) }}"
        ));
        let legacy =
            CodeHandle::compile_named(&scalar_source, &scalar_source["Entry"], Limits::default())
                .unwrap();
        assert_eq!(
            legacy
                .function(legacy.find_function("Entry").unwrap())
                .owned_scalar_result,
            None,
            "the old scalar selector retains its separate role"
        );
    }

    // array.first has an existing Unknown checker result. An empty array
    // yields a real Null, which the old evaluator preserves despite float
    // declaration metadata. Compare that direct old call to the complete
    // checked ordinary source path, without inventing coercion or replay.
    let declaration = "fn Entry(text: str, value: float): float! { marker = 1\nreturn ok(value) }";
    let source_functions = functions(declaration);
    let mut old = production_interpreter(declaration);
    let unknown = crate::stdlib::array::eval("first", &[Value::Array(Vec::new())], Span::default())
        .unwrap()
        .unwrap();
    let (text, buffer) = string_input(PAYLOAD, 4096);
    let legacy = old
        .call_function_direct_flow(
            source_functions["Entry"].clone(),
            vec![text, unknown],
            Span::default(),
            0,
        )
        .unwrap()
        .into_call_value(Span::default(), "function")
        .unwrap();
    let complete = format!("{declaration}\nfn RequireNull(value): null! {{ if (value == null) {{ return ok(null) }}\nreturn err(\"Unknown primitive was changed\") }}\nfn main(): null! {{ items = []\nunknown = array.first(items)\nactual = Entry(\"{PAYLOAD}\", unknown)?\nRequireNull(actual)?\nreturn ok(null) }}");
    let program = Parser::new(Lexer::new(&complete).tokenize().unwrap())
        .parse_program()
        .unwrap();
    crate::checker::Checker::new().check(&program).unwrap();
    crate::cli::run_source("owned-primitive-unknown-null.ku", &complete).unwrap();
    assert_eq!(
        primitive_snapshot(&legacy, PrimitiveResultKind::Null, &[buffer])
            .unwrap()
            .leaf,
        PrimitiveLeaf::Null
    );
    assert_eq!(old.call_depth, 0);
    let code = CodeHandle::compile_owned_named(
        &source_functions,
        &source_functions["Entry"],
        Limits::default(),
    )
    .unwrap();
    assert_eq!(
        code.function(code.find_function("Entry").unwrap())
            .owned_scalar_result,
        Some(PrimitiveResultKind::Float),
        "declared kind remains the static relay contract, rather than coercion of the actual Null"
    );

    for (element, syntax) in [
        (Value::Array(vec![Value::Int(1), Value::Int(2)]), "[1, 2]"),
        (
            Value::Object(HashMap::from([("answer".to_owned(), Value::Int(7))])),
            "{ answer: 7 }",
        ),
    ] {
        let complete = format!("{declaration}\nfn main(): null! {{ items = []\nitems = array.push(items, {syntax})\nunknown = array.first(items)\nactual = Entry(\"{PAYLOAD}\", unknown)?\nreturn ok(null) }}");
        let program = Parser::new(Lexer::new(&complete).tokenize().unwrap())
            .parse_program()
            .unwrap();
        crate::checker::Checker::new().check(&program).unwrap();
        let mut host = production_interpreter(declaration);
        // This is the real existing builtin conversion used by the legal
        // source above. No test clones a Value to manufacture an owner.
        let unknown =
            crate::stdlib::array::eval("first", &[Value::Array(vec![element])], Span::default())
                .unwrap()
                .unwrap();
        let opaque = |value: &Value| match value {
            Value::Array(values) => vec![values.as_ptr() as usize, values.capacity()],
            Value::Object(fields) => {
                let (key, _) = fields.get_key_value("answer").unwrap();
                vec![
                    fields.capacity(),
                    key as *const String as usize,
                    key.as_ptr() as usize,
                    key.capacity(),
                ]
            }
            value => panic!("the actual Unknown conversion must remain opaque: {value:?}"),
        };
        let before = opaque(&unknown);
        let (text, buffer) = string_input(PAYLOAD, 4096);
        let mut args = vec![text, unknown];
        assert!(try_execute_owned(&mut host, &source_functions["Entry"], &mut args, 0).is_none());
        assert_eq!(
            (host.steps, host.call_depth),
            (0, 0),
            "opaque actual parameters select whole legacy before body effects"
        );
        let Value::String(text) = &args[0] else {
            unreachable!()
        };
        assert_eq!(
            (text.as_ptr() as usize, text.capacity()),
            (buffer.pointer, buffer.capacity)
        );
        assert_eq!(
            opaque(&args[1]),
            before,
            "the sole actual Array/Map argument has not moved or been cloned by selection"
        );
        let result = host
            .call_function("Entry", args, Span::default(), 0)
            .unwrap();
        let Value::Result { ok: true, value } = &result else {
            panic!("the original opaque legacy Result contract")
        };
        match value.as_ref() {
            Value::Array(values) => assert_eq!(values.as_slice(), &[Value::Int(1), Value::Int(2)]),
            Value::Object(fields) => assert_eq!(fields.get("answer"), Some(&Value::Int(7))),
            value => panic!("whole fallback must preserve the actual opaque payload: {value:?}"),
        }
        assert_eq!(host.call_depth, 0);
        crate::cli::run_source("owned-primitive-unknown-opaque.ku", &complete).unwrap();
    }
}

#[derive(Clone, Copy)]
struct LocalMetadata {
    owner: PrimitiveBox,
    valid_error: bool,
    message_input: Option<usize>,
}

fn local_metadata(value: &Value, buffers: &[Buffer]) -> Option<LocalMetadata> {
    let Value::Result { ok, value: leaf } = value else {
        return None;
    };
    let address = leaf.as_ref() as *const Value as usize;
    if *ok {
        let leaf = match leaf.as_ref() {
            Value::Int(value) => PrimitiveLeaf::Int(*value),
            Value::Float(value) => PrimitiveLeaf::Float(value.to_bits()),
            Value::Bool(value) => PrimitiveLeaf::Bool(*value),
            Value::Null => PrimitiveLeaf::Null,
            _ => return None,
        };
        return Some(LocalMetadata {
            owner: PrimitiveBox {
                address,
                leaf,
                error: None,
            },
            valid_error: true,
            message_input: None,
        });
    }
    let actual = error_payload(value)?;
    let Value::Object(fields) = leaf.as_ref() else {
        return None;
    };
    let valid_error = matches!(fields.get("domain"), Some(Value::String(domain)) if domain == "ku")
        && matches!(fields.get("code"), Some(Value::String(code)) if code == "err");
    let message_input = match fields.get("message") {
        Some(Value::String(message)) => buffers.iter().position(|buffer| message == buffer.text),
        _ => None,
    };
    Some(LocalMetadata {
        owner: PrimitiveBox {
            address,
            leaf: PrimitiveLeaf::Error,
            error: Some(actual),
        },
        valid_error,
        message_input,
    })
}

fn checked_local_metadata(actual: LocalMetadata) -> PrimitiveBox {
    assert!(
        actual.valid_error,
        "source local failures keep the ku/err schema"
    );
    if actual.owner.error.is_some() {
        assert!(actual.message_input.is_some());
    }
    actual.owner
}

struct LocalCensus {
    env: Vec<PrimitiveBox>,
    live: Vec<PrimitiveBox>,
}

fn local_census(machine: &Machine, buffers: &[Buffer]) -> LocalCensus {
    // These fixtures have at most three Result bindings per call. Allocate
    // the metadata backing before taking any source BindingCell lock. The
    // fold callback only reads Copy metadata and pushes within this capacity.
    let mut metadata = Vec::with_capacity(machine.calls.len() * 4);
    let mut env_charge = AwaitPayloadCharge::default();
    for frame in machine.calls.as_slice() {
        let has_result = frame.env.fold_owned_values(false, |has_result, value| {
            if let Some(actual) = local_metadata(value, buffers) {
                metadata.push(actual);
            }
            has_result || matches!(value, Value::Result { .. })
        });
        assert!(
            !has_result || frame.has_selected_result_bindings,
            "every real Env Result activates its frame census through the production Bind"
        );
        let charge = frame.env.selected_scalar_result_charge();
        env_charge = sum_payload_charge(env_charge, charge);
    }
    let env = metadata
        .into_iter()
        .map(checked_local_metadata)
        .collect::<Vec<_>>();
    let mut live = env.clone();
    let mut read = |value: &Value| {
        if let Some(actual) = local_metadata(value, buffers) {
            live.push(checked_local_metadata(actual));
        }
    };
    for value in machine.values.as_slice() {
        read(value);
    }
    if let Some(Ok(Flow::Return { value, .. } | Flow::Fail { value, .. })) = &machine.pending {
        read(value);
    }
    for exit in machine.exits.as_slice() {
        if let Some(Ok(Flow::Return { value, .. } | Flow::Fail { value, .. })) =
            exit.state.outcome()
        {
            read(value);
        }
    }
    for frame in machine.finally.frames.as_slice() {
        if let Some(Ok(Flow::Return { value, .. } | Flow::Fail { value, .. })) = &frame.primary {
            read(value);
        }
    }
    assert!(
        machine.awaiting.is_none(),
        "S7 remains the synchronous local slice"
    );
    let mut addresses = std::collections::HashSet::with_capacity(live.len());
    for owner in &live {
        assert!(
            addresses.insert(owner.address),
            "one physical owning location per Box"
        );
    }
    let (env_count, env_bytes) = local_error_totals(&env);
    assert_eq!(
        (
            env_charge.box_count,
            env_charge.box_bytes,
            env_charge.error_leaf_count,
            env_charge.error_leaf_bytes
        ),
        (
            env.len(),
            env.len() * std::mem::size_of::<Value>(),
            env_count,
            env_bytes
        )
    );
    let (errors, error_bytes) = local_error_totals(&live);
    assert_eq!(
        (
            machine.result_wrapper_capacity,
            machine.result_wrapper_bytes,
            machine.error_leaf_capacity,
            machine.error_leaf_bytes
        ),
        (
            live.len(),
            live.len() * std::mem::size_of::<Value>(),
            errors,
            error_bytes
        )
    );
    assert_eq!(
        (
            machine.metrics.retained_result_wrapper_capacity,
            machine.metrics.retained_result_wrapper_bytes,
            machine.metrics.retained_error_leaf_capacity,
            machine.metrics.retained_error_leaf_bytes
        ),
        (
            live.len(),
            live.len() * std::mem::size_of::<Value>(),
            errors,
            error_bytes
        )
    );
    LocalCensus { env, live }
}

fn local_error_totals(owners: &[PrimitiveBox]) -> (usize, usize) {
    (
        owners
            .iter()
            .filter_map(|owner| owner.error)
            .map(|error| error.count)
            .sum(),
        owners
            .iter()
            .filter_map(|owner| owner.error)
            .map(|error| error.bytes)
            .sum(),
    )
}

#[derive(Default)]
struct LocalProof {
    live: Vec<PrimitiveBox>,
    history: Vec<PrimitiveBox>,
    last_candidate: Option<PrimitiveBox>,
    peak_env: usize,
    peak_live: usize,
}

fn observe_local(machine: &Machine, buffers: &[Buffer], proof: &mut LocalProof) -> LocalCensus {
    let census = local_census(machine, buffers);
    let previous: HashMap<_, _> = proof
        .live
        .iter()
        .map(|owner| (owner.address, owner))
        .collect();
    assert_eq!(previous.len(), proof.live.len());
    if let Some(candidate) = machine
        .values
        .as_slice()
        .last()
        .and_then(|value| local_metadata(value, buffers))
    {
        if !previous.contains_key(&candidate.owner.address) {
            proof.last_candidate = Some(checked_local_metadata(candidate));
        }
    }
    for actual in &census.live {
        if let Some(old) = previous.get(&actual.address) {
            assert_eq!(
                **old, *actual,
                "a continuously live owner retains its backing"
            );
        } else {
            proof.history.push(*actual);
        }
    }
    proof.peak_env = proof.peak_env.max(census.env.len());
    proof.peak_live = proof.peak_live.max(census.live.len());
    proof.live = census.live.clone();
    census
}

fn at_local_clone(machine: &Machine) -> bool {
    machine.pending.is_none()
        && matches!(
            machine.work.as_slice().last(),
            Some(Work::CloneSelectedScalarResult(_, _))
        )
}

fn local_machine(source: &str, kind: PrimitiveResultKind) -> Machine {
    let source_functions = functions(source);
    let entry = &source_functions["Entry"];
    assert!(
        code::eligible_owned_named(&source_functions, entry, Limits::default()),
        "selected S7 positive: {source}"
    );
    let code =
        CodeHandle::compile_owned_named(&source_functions, entry, Limits::default()).unwrap();
    assert_eq!(
        code.function(code.find_function("Entry").unwrap())
            .owned_scalar_result,
        Some(kind)
    );
    Machine::from_code(code, Limits::default(), 0)
}

fn finish_local(
    machine: &mut Machine,
    host: &mut Interpreter,
    chunk: usize,
    buffers: &[Buffer],
    proof: &mut LocalProof,
) -> KuResult<Value> {
    for _ in 0..100_000 {
        match machine
            .dispatch_chunk(NonZeroUsize::new(chunk).unwrap(), host)
            .unwrap()
        {
            ChunkProgress::More { popped } => {
                assert_eq!(popped, chunk);
                assert_eq!(
                    machine.string_payload_bytes,
                    buffers.iter().map(|buffer| buffer.capacity).sum::<usize>()
                );
                observe_local(machine, buffers, proof);
            }
            ChunkProgress::Waiting { .. } => panic!("the S7 synchronous fixture cannot await"),
            ChunkProgress::Finished { popped, result } => {
                assert!(popped <= chunk);
                let result =
                    result.and_then(|flow| flow.into_call_value(Span::default(), "function"));
                if chunk == 1 {
                    if let Ok(value) = &result {
                        let actual =
                            checked_local_metadata(local_metadata(value, buffers).unwrap());
                        assert!(proof.live.contains(&actual), "the final real Box was independently observed before physical teardown");
                        assert_eq!(proof.last_candidate, Some(actual));
                    }
                }
                errors_drained(machine);
                return result;
            }
        }
    }
    panic!("the finite S7 local source did not finish");
}

fn assert_distinct_local_clone(original: PrimitiveBox, cloned: PrimitiveBox) {
    assert_ne!(
        original.address, cloned.address,
        "old Env.get_owning remains a clone, never a take"
    );
    assert_eq!(original.leaf, cloned.leaf);
    match (original.error, cloned.error) {
        (Some(a), Some(b)) => {
            for index in 0..3 {
                assert_ne!(a.entries[index], b.entries[index]);
            }
            for index in 0..6 {
                assert_ne!(
                    a.buffers[index].0, b.buffers[index].0,
                    "each nonempty resident error String has a distinct real clone allocation"
                );
            }
        }
        (None, None) => {}
        _ => panic!("cloning preserves the real success/failure shape"),
    }
}

#[test]
fn selected_scalar_locals_clone_real_env_boxes_and_each_error_buffer_across_chunks() {
    for (kind, ty, literal) in PRIMITIVE_CASES {
        for error in [false, true] {
            let leaf = if error {
                "err(text)".to_string()
            } else {
                format!("ok({literal})")
            };
            for helper in [false, true] {
                let prefix = if helper {
                    format!("fn Leaf(text: str): {ty}! {{ return {leaf} }}\n")
                } else {
                    String::new()
                };
                let rhs = if helper {
                    "Leaf(text)".to_string()
                } else {
                    leaf.clone()
                };
                let source = format!("{prefix}fn Entry(text: str): {ty}! {{ r: {ty}! = {rhs}\nr\nr\nr = r\nreturn r }}");
                for chunk in [1, 7, 256] {
                    let mut machine = local_machine(&source, kind);
                    let mut host = production_interpreter(&source);
                    let (text, buffer) = string_input(PAYLOAD, 4096);
                    let mut args = vec![text];
                    let mut proof = LocalProof::default();
                    machine
                        .start_owned_once("Entry", &mut args, 0, false)
                        .unwrap();
                    if chunk == 1 {
                        assert_eq!(machine.calls.len(), 1);
                        assert!(
                            !machine.calls.as_slice()[0].has_selected_result_bindings,
                            "real Entry String parameters do not activate the Result Env census"
                        );
                        let entered = observe_local(&machine, &[buffer], &mut proof);
                        assert!(entered.env.is_empty() && entered.live.is_empty());
                        pause_at(&mut machine, &mut host, at_local_clone);
                        let before = observe_local(&machine, &[buffer], &mut proof);
                        assert_eq!(before.env.len(), 1);
                        let original = before.env[0];
                        check_primitive_live_budget(&mut machine);
                        assert!(matches!(
                            machine
                                .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut host)
                                .unwrap(),
                            ChunkProgress::More { popped: 1 }
                        ));
                        let after = observe_local(&machine, &[buffer], &mut proof);
                        assert_eq!((after.env.len(), after.live.len()), (1, 2));
                        assert_eq!(after.env[0], original);
                        let cloned = after
                            .live
                            .iter()
                            .find(|owner| owner.address != original.address)
                            .copied()
                            .unwrap();
                        assert_distinct_local_clone(original, cloned);
                        check_primitive_live_budget(&mut machine);
                    }
                    let result =
                        finish_local(&mut machine, &mut host, chunk, &[buffer], &mut proof)
                            .unwrap();
                    if error {
                        assert_eq!(outcome_buffer(&result, &[buffer]).0, 0);
                    } else {
                        assert_eq!(
                            local_metadata(&result, &[buffer]).unwrap().owner.leaf,
                            expected_primitive(kind)
                        );
                    }
                    assert_eq!(
                        (
                            machine.metrics.peak_result_wrapper_capacity,
                            machine.metrics.peak_result_wrapper_bytes
                        ),
                        (2, 2 * std::mem::size_of::<Value>())
                    );
                    assert_eq!(machine.metrics.peak_string_payload_bytes, buffer.capacity);
                    if chunk == 1 {
                        assert_eq!(proof.peak_env, 1);
                        assert!(proof.history.len() >= 5);
                    }
                }
            }
        }
    }
}

fn finish_deep_local(
    machine: &mut Machine,
    host: &mut Interpreter,
    kind: PrimitiveResultKind,
    n: i64,
    buffers: &[Buffer],
    proof: &mut LocalProof,
) -> KuResult<Value> {
    if n == 0 {
        return finish_local(machine, host, 1, buffers, proof);
    }
    let mut full_depth_observed = false;
    let mut observed_depth = 0;
    for _ in 0..100_000 {
        if full_depth_observed && machine.calls.len() <= 32 {
            // Resume continuous single-step observations before the final
            // returned Box can leave the machine. Large chunks below never
            // infer continuous ownership from a reused allocation address.
            proof.live.clear();
            proof.last_candidate = None;
            return finish_local(machine, host, 1, buffers, proof);
        }
        let depth = machine.calls.len();
        if !full_depth_observed
            && depth != observed_depth
            && depth % 64 == 0
            && machine
                .calls
                .as_slice()
                .last()
                .unwrap()
                .has_selected_result_bindings
        {
            let census = local_census(machine, buffers);
            assert_eq!(census.env.len(), depth);
            assert!(census
                .env
                .iter()
                .all(|owner| owner.leaf == expected_primitive(kind)));
            proof.peak_env = proof.peak_env.max(census.env.len());
            proof.peak_live = proof.peak_live.max(census.live.len());
            observed_depth = depth;
            if depth == 512 {
                // All 512 keep bindings came from real production Bind work.
                // The independent census verifies every simultaneously live
                // Box, and all four budget entrypoints see its actual fee.
                check_primitive_live_budget(machine);
                full_depth_observed = true;
            }
        }
        let chunk = if full_depth_observed { 256 } else { 1 };
        match machine
            .dispatch_chunk(NonZeroUsize::new(chunk).unwrap(), host)
            .unwrap()
        {
            ChunkProgress::More { popped } => {
                assert_eq!(popped, chunk);
                assert_eq!(
                    machine.string_payload_bytes,
                    buffers.iter().map(|buffer| buffer.capacity).sum::<usize>()
                );
                if full_depth_observed {
                    let census = local_census(machine, buffers);
                    proof.peak_env = proof.peak_env.max(census.env.len());
                    proof.peak_live = proof.peak_live.max(census.live.len());
                }
            }
            ChunkProgress::Waiting { .. } => panic!("the S7 synchronous fixture cannot await"),
            ChunkProgress::Finished { .. } => {
                panic!(
                    "the deep run must expose all 512 bindings and its final single-step teardown"
                )
            }
        }
    }
    panic!("the finite deep S7 local source did not finish");
}

#[test]
fn selected_scalar_locals_charge_every_live_call_at_512_reject_first_513_and_reuse() {
    for (kind, ty, literal) in PRIMITIVE_CASES {
        for error in [false, true] {
            let leaf = if error {
                "err(text)".to_string()
            } else {
                format!("ok({literal})")
            };
            let source = format!("fn Entry(text: str, n: int): {ty}! {{ keep: {ty}! = ok({literal})\nif (n <= 0) {{ r: {ty}! = {leaf}\nr\nr\nr = r\nreturn r }}\nr: {ty}! = Entry(text, n - 1)\nr\nr\nr = r\nreturn r }}");
            let mut machine = local_machine(&source, kind);
            let mut host = production_interpreter(&source);
            for n in [511, 512, 0] {
                if n != 511 {
                    machine.prepare_next_run().unwrap();
                }
                let entered_before = machine.metrics.entered_calls;
                let retired_before = machine.metrics.retired_calls;
                let error_peak_before = (
                    machine.metrics.peak_error_leaf_capacity,
                    machine.metrics.peak_error_leaf_bytes,
                );
                let (text, buffer) = string_input(PAYLOAD, 4096);
                let mut args = vec![text, Value::Int(n)];
                let mut proof = LocalProof::default();
                machine
                    .start_owned_once("Entry", &mut args, 0, false)
                    .unwrap();
                let result =
                    finish_deep_local(&mut machine, &mut host, kind, n, &[buffer], &mut proof);
                if n == 512 {
                    assert_eq!(
                        result.unwrap_err().code.as_deref(),
                        Some("call_depth_exceeded")
                    );
                    assert_eq!(
                        (
                            machine.metrics.entered_calls - entered_before,
                            machine.metrics.peak_calls,
                            proof.peak_env
                        ),
                        (512, 512, 512)
                    );
                    assert_eq!(
                        (
                            machine.metrics.peak_error_leaf_capacity,
                            machine.metrics.peak_error_leaf_bytes,
                        ),
                        error_peak_before,
                        "rejecting the first 513th call cannot replay the real err leaf or increase its cumulative peak"
                    );
                } else {
                    let result = result.unwrap();
                    if error {
                        outcome_buffer(&result, &[buffer]);
                    } else {
                        assert_eq!(
                            local_metadata(&result, &[buffer]).unwrap().owner.leaf,
                            expected_primitive(kind)
                        );
                    }
                    assert_eq!(
                        machine.metrics.entered_calls - entered_before,
                        (n + 1) as usize
                    );
                    assert!(proof.peak_env >= (n + 1) as usize);
                }
                assert_eq!(
                    machine.metrics.retired_calls - retired_before,
                    machine.metrics.entered_calls - entered_before
                );
                assert_eq!(machine.metrics.retired_calls, machine.metrics.entered_calls);
                errors_drained(&machine);
                assert_eq!(host.call_depth, 0);
            }
        }
    }
}

fn unlimited_local_clone_policy() -> AwaitPayloadPolicy {
    AwaitPayloadPolicy {
        max_count: usize::MAX,
        max_bytes: usize::MAX,
        input_string_capacity: 0,
        span: Span::default(),
    }
}

fn env_local_metadata(env: &Env, buffers: &[Buffer]) -> Option<LocalMetadata> {
    env.fold_owned_values(None, |first, value| {
        first.or_else(|| local_metadata(value, buffers))
    })
}

fn sparse_local_error() -> Value {
    let mut fields = HashMap::with_capacity(64);
    for (name, text) in [("domain", "ku"), ("code", "err"), ("message", PAYLOAD)] {
        let mut key = String::with_capacity(256);
        key.push_str(name);
        let mut value = String::with_capacity(1024);
        value.push_str(text);
        fields.insert(key, Value::String(value));
    }
    Value::Result {
        ok: false,
        value: Box::new(Value::Object(fields)),
    }
}

#[test]
fn selected_scalar_local_clone_budgets_include_resident_and_actual_sparse_map_rejection() {
    for (kind, ty, literal) in PRIMITIVE_CASES {
        for error in [false, true] {
            let leaf = if error {
                "err(text)".to_string()
            } else {
                format!("ok({literal})")
            };
            let source = format!("fn Entry(text: str): {ty}! {{ r: {ty}! = {leaf}\nreturn r }}");
            for rejection in [None, Some("FrameLimit"), Some("StorageLimit")] {
                let mut machine = local_machine(&source, kind);
                let mut host = production_interpreter(&source);
                let (text, buffer) = string_input(PAYLOAD, 4096);
                let mut args = vec![text];
                let mut proof = LocalProof::default();
                machine
                    .start_owned_once("Entry", &mut args, 0, false)
                    .unwrap();
                pause_at(&mut machine, &mut host, at_local_clone);
                let before = observe_local(&machine, &[buffer], &mut proof);
                assert_eq!(before.env.len(), 1);
                let resident = before.env[0];
                machine.reserve_values(1, Span::default()).unwrap();
                check_primitive_live_budget(&mut machine);
                // Measure the genuine kernel's destination, then retire it.
                // This is an operation-local clone, never a fabricated VM owner.
                let mut observed = AwaitPayloadCharge::default();
                let (measured, candidate) = machine
                    .calls
                    .as_slice()
                    .last()
                    .unwrap()
                    .env
                    .with_value("r", Span::default(), |value| {
                        clone_selected_scalar_result(
                            value,
                            unlimited_local_clone_policy(),
                            &mut observed,
                        )
                    })
                    .unwrap();
                let measured_owner =
                    checked_local_metadata(local_metadata(&measured, &[buffer]).unwrap());
                assert_distinct_local_clone(resident, measured_owner);
                assert_eq!(
                    candidate,
                    AwaitPayloadCharge::for_value(&measured, 0).unwrap()
                );
                assert_eq!(observed, candidate);
                drop(measured);
                let saved = machine.value_limits;
                machine.value_limits.max_frames = live_value_count(&machine) + candidate.count
                    - usize::from(rejection == Some("FrameLimit"));
                machine.value_limits.max_storage_bytes = live_value_bytes(&machine)
                    + candidate.bytes
                    - usize::from(rejection == Some("StorageLimit"));
                assert!(matches!(
                    machine
                        .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut host)
                        .unwrap(),
                    ChunkProgress::More { popped: 1 }
                ));
                let after = observe_local(&machine, &[buffer], &mut proof);
                assert_eq!(after.env, before.env);
                if rejection.is_some() {
                    assert_eq!(
                        after.live.len(),
                        1,
                        "the rejected candidate is never published"
                    );
                } else {
                    assert_eq!(after.live.len(), 2);
                    check_primitive_live_budget(&mut machine);
                }
                machine.value_limits = saved;
                let result = finish_local(&mut machine, &mut host, 1, &[buffer], &mut proof);
                if let Some(reason) = rejection {
                    assert!(result.unwrap_err().message.contains(reason));
                    assert_eq!(machine.metrics.peak_result_wrapper_capacity, 1);
                } else {
                    let result = result.unwrap();
                    if error {
                        outcome_buffer(&result, &[buffer]);
                    } else {
                        assert_eq!(
                            local_metadata(&result, &[buffer]).unwrap().owner.leaf,
                            expected_primitive(kind)
                        );
                    }
                }
            }
        }
    }
    // Private real sparse ErrorObject shape, not a claim that source err grows
    // its Map. The production kernel checks planned -> cloned actual Map ->
    // Box in that order. No allocation event exposes the partial Map here.
    let buffer = Buffer {
        pointer: 0,
        capacity: 0,
        text: PAYLOAD,
    };
    let mut env = Env::new();
    env.define_owned("r".to_string(), sparse_local_error(), true, Span::default())
        .unwrap();
    let original = checked_local_metadata(env_local_metadata(&env, &[buffer]).unwrap());
    assert!(original.error.unwrap().count > 3);
    let planned = AwaitPayloadCharge::planned_error_object("ku", "err", PAYLOAD.len()).unwrap();
    let mut observed = AwaitPayloadCharge::default();
    let (clone, actual) = env
        .with_value("r", Span::default(), |source| {
            clone_selected_scalar_result(source, unlimited_local_clone_policy(), &mut observed)
        })
        .unwrap();
    let cloned = checked_local_metadata(local_metadata(&clone, &[buffer]).unwrap());
    assert_distinct_local_clone(original, cloned);
    assert_eq!(actual, AwaitPayloadCharge::for_value(&clone, 0).unwrap());
    assert_eq!(observed, actual);
    assert!(actual.count > planned.count && actual.bytes > planned.bytes);
    drop(clone);
    for (count, bytes, reason) in [
        (planned.count - 1, usize::MAX, "FrameLimit"),
        (usize::MAX, planned.bytes - 1, "StorageLimit"),
        (planned.count, usize::MAX, "FrameLimit"),
        (usize::MAX, planned.bytes, "StorageLimit"),
    ] {
        let policy = AwaitPayloadPolicy {
            max_count: count,
            max_bytes: bytes,
            ..unlimited_local_clone_policy()
        };
        let mut observed = AwaitPayloadCharge::default();
        let result = env.with_value("r", Span::default(), |source| {
            clone_selected_scalar_result(source, policy, &mut observed)
        });
        assert!(result
            .expect_err("real clone candidate is rejected")
            .message
            .contains(reason));
        let expected = if count < planned.count || bytes < planned.bytes {
            AwaitPayloadCharge::default()
        } else {
            AwaitPayloadCharge {
                count: actual.error_leaf_count,
                bytes: actual.error_leaf_bytes,
                error_leaf_count: actual.error_leaf_count,
                error_leaf_bytes: actual.error_leaf_bytes,
                ..AwaitPayloadCharge::default()
            }
        };
        assert_eq!(
            observed, expected,
            "actual clone metadata never invents a Result Box"
        );
        assert_eq!(
            checked_local_metadata(env_local_metadata(&env, &[buffer]).unwrap()),
            original
        );
    }
    let exact = AwaitPayloadPolicy {
        max_count: actual.count,
        max_bytes: actual.bytes,
        ..unlimited_local_clone_policy()
    };
    let mut observed = AwaitPayloadCharge::default();
    let (clone, charge) = env
        .with_value("r", Span::default(), |source| {
            clone_selected_scalar_result(source, exact, &mut observed)
        })
        .unwrap();
    assert_eq!(charge, AwaitPayloadCharge::for_value(&clone, 0).unwrap());
    assert_eq!(observed, charge);
    drop(clone);
    drop(env);

    // Move a privately constructed sparse owner into the real typed-local
    // Bind slot. The legal source selects S7; this enlarged Map is a private
    // shape contract, rather than a source err allocation claim.
    let source = "fn Entry(text: str): int! { r: int! = ok(7)\nreturn r }";
    for count_rejection in [false, true] {
        let mut machine = local_machine(source, PrimitiveResultKind::Int);
        let mut host = production_interpreter(source);
        let (text, buffer) = string_input(PAYLOAD, 4096);
        let mut args = vec![text];
        let mut proof = LocalProof::default();
        machine
            .start_owned_once("Entry", &mut args, 0, false)
            .unwrap();
        pause_at(&mut machine, &mut host, |m| {
            matches!(m.work.as_slice().last(), Some(Work::Bind(_, _, true, _)))
                && m.values
                    .as_slice()
                    .last()
                    .is_some_and(|value| matches!(value, Value::Result { .. }))
        });
        drop(machine.values.pop().unwrap());
        push(&mut machine.values, sparse_local_error());
        machine.synchronize_result_wrapper_charge();
        pause_at(&mut machine, &mut host, at_local_clone);
        let before = observe_local(&machine, &[buffer], &mut proof);
        assert_eq!((before.env.len(), before.live.len()), (1, 1));
        let resident = before.env[0];
        let mut observed = AwaitPayloadCharge::default();
        let (measured, candidate) = machine
            .calls
            .as_slice()
            .last()
            .unwrap()
            .env
            .with_value("r", Span::default(), |value| {
                clone_selected_scalar_result(value, unlimited_local_clone_policy(), &mut observed)
            })
            .unwrap();
        let actual = checked_local_metadata(local_metadata(&measured, &[buffer]).unwrap());
        assert_distinct_local_clone(resident, actual);
        assert_eq!(
            candidate,
            AwaitPayloadCharge::for_value(&measured, 0).unwrap()
        );
        assert_eq!(observed, candidate);
        assert!(candidate.count > planned.count && candidate.bytes > planned.bytes);
        drop(measured);
        machine.reserve_values(1, Span::default()).unwrap();
        let saved = machine.value_limits;
        if count_rejection {
            machine.value_limits.max_frames = live_value_count(&machine) + planned.count;
        } else {
            machine.value_limits.max_storage_bytes = live_value_bytes(&machine) + planned.bytes;
        }
        let policy = machine.await_payload_policy(Span::default()).unwrap();
        assert_eq!(
            if count_rejection {
                policy.max_count
            } else {
                policy.max_bytes
            },
            if count_rejection {
                planned.count
            } else {
                planned.bytes
            }
        );
        assert!(matches!(
            machine
                .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut host)
                .unwrap(),
            ChunkProgress::More { popped: 1 }
        ));
        let after = observe_local(&machine, &[buffer], &mut proof);
        assert_eq!(after.env, before.env);
        assert_eq!(
            after.live, before.live,
            "the rejected actual Map is not published"
        );
        let original = resident.error.unwrap();
        let peak = (
            original.count + candidate.error_leaf_count,
            original.bytes + candidate.error_leaf_bytes,
        );
        assert_eq!(
            (
                machine.metrics.peak_error_leaf_capacity,
                machine.metrics.peak_error_leaf_bytes
            ),
            peak
        );
        assert_eq!(
            (
                machine.metrics.peak_result_wrapper_capacity,
                machine.metrics.peak_result_wrapper_bytes
            ),
            (1, std::mem::size_of::<Value>()),
            "actual Map rejection precedes the candidate Box"
        );
        machine.value_limits = saved;
        let error = finish_local(&mut machine, &mut host, 1, &[buffer], &mut proof).unwrap_err();
        assert!(error.message.contains(if count_rejection {
            "FrameLimit"
        } else {
            "StorageLimit"
        }));
        assert_eq!(
            (
                machine.metrics.peak_error_leaf_capacity,
                machine.metrics.peak_error_leaf_bytes
            ),
            peak
        );
        assert!(
            machine.metrics.peak_backing_bytes_upper_bound
                >= buffer.capacity + std::mem::size_of::<Value>() + peak.1
        );
        assert_eq!(host.call_depth, 0);
    }
}

#[test]
fn selected_scalar_local_bind_assign_failures_shadow_and_capture_census_preserve_real_owners() {
    for (kind, ty, literal) in PRIMITIVE_CASES {
        for error in [false, true] {
            let leaf = if error {
                "err(text)".to_string()
            } else {
                format!("ok({literal})")
            };
            let source =
                format!("fn Entry(text: str): {ty}! {{ r: {ty}! = {leaf}\nr = r\nreturn r }}");
            for failure in [None, Some("duplicate"), Some("immutable")] {
                let mut machine = local_machine(&source, kind);
                let mut host = production_interpreter(&source);
                let (text, buffer) = string_input(PAYLOAD, 4096);
                let mut args = vec![text];
                let mut proof = LocalProof::default();
                machine
                    .start_owned_once("Entry", &mut args, 0, false)
                    .unwrap();
                if failure == Some("immutable") {
                    pause_at(&mut machine, &mut host, |m| {
                        matches!(m.work.as_slice().last(), Some(Work::Bind(_, _, true, _)))
                            && m.values
                                .as_slice()
                                .last()
                                .is_some_and(|v| matches!(v, Value::Result { .. }))
                    });
                    // Private contract fixture: change only the mutable bit
                    // on the real declaration continuation. The production
                    // Bind stores the sole emitted RHS and activates its Env
                    // census; no Value clone or owner token is invented.
                    let Work::Bind(name, _, declare, span) = machine.work.pop().unwrap() else {
                        panic!("the real declaration follows its emitted RHS")
                    };
                    assert!(declare);
                    push(&mut machine.work, Work::Bind(name, false, declare, span));
                    assert!(matches!(
                        machine
                            .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut host)
                            .unwrap(),
                        ChunkProgress::More { popped: 1 }
                    ));
                }
                pause_at(&mut machine, &mut host, at_local_clone);
                let original = observe_local(&machine, &[buffer], &mut proof).env[0];
                machine
                    .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut host)
                    .unwrap();
                let before_bind = observe_local(&machine, &[buffer], &mut proof);
                assert_eq!((before_bind.env.len(), before_bind.live.len()), (1, 2));
                let rhs = before_bind
                    .live
                    .iter()
                    .find(|owner| owner.address != original.address)
                    .copied()
                    .unwrap();
                assert_distinct_local_clone(original, rhs);
                check_primitive_live_budget(&mut machine);
                if failure == Some("duplicate") {
                    // Private duplicate-definition failure uses the same real
                    // RHS and existing slot, not a checker-invalid source.
                    let Work::Bind(name, mutable, _, span) = machine.work.pop().unwrap() else {
                        panic!("the real assign continuation follows its RHS clone")
                    };
                    push(&mut machine.work, Work::Bind(name, mutable, true, span));
                }
                machine
                    .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut host)
                    .unwrap();
                let after = observe_local(&machine, &[buffer], &mut proof);
                assert_eq!((after.env.len(), after.live.len()), (1, 1));
                assert_eq!(after.env[0], if failure.is_some() { original } else { rhs });
                let result = finish_local(&mut machine, &mut host, 1, &[buffer], &mut proof);
                if let Some(failure) = failure {
                    assert!(result
                        .unwrap_err()
                        .message
                        .contains(if failure == "duplicate" {
                            "already defined"
                        } else {
                            "immutable"
                        }));
                } else {
                    let result = result.unwrap();
                    if error {
                        outcome_buffer(&result, &[buffer]);
                    } else {
                        assert_eq!(
                            local_metadata(&result, &[buffer]).unwrap().owner.leaf,
                            expected_primitive(kind)
                        );
                    }
                }
            }
        }
        let shadow = format!("fn Entry(text: str): {ty}! {{ r: {ty}! = ok({literal})\nif (true) {{ r: {ty}! = ok({literal})\nr }}\nreturn r }}");
        let mut machine = local_machine(&shadow, kind);
        let mut host = production_interpreter(&shadow);
        let (text, buffer) = string_input(PAYLOAD, 4096);
        let mut args = vec![text];
        let mut proof = LocalProof::default();
        machine
            .start_owned_once("Entry", &mut args, 0, false)
            .unwrap();
        pause_at(&mut machine, &mut host, |m| {
            at_local_clone(m) && m.result_wrapper_capacity == 2
        });
        let census = observe_local(&machine, &[buffer], &mut proof);
        assert_eq!(census.env.len(), 2);
        assert_ne!(census.env[0].address, census.env[1].address);
        let captured = machine.calls.as_slice().last().unwrap().env.clone();
        assert_eq!(
            captured.fold_owned_values(0usize, |n, value| n + usize::from(
                local_metadata(value, &[buffer]).is_some()
            )),
            0
        );
        assert_eq!(
            captured.selected_scalar_result_charge(),
            AwaitPayloadCharge::default()
        );
        drop(captured);
        let result = finish_local(&mut machine, &mut host, 1, &[buffer], &mut proof).unwrap();
        assert_eq!(
            local_metadata(&result, &[buffer]).unwrap().owner.leaf,
            expected_primitive(kind)
        );
        assert_eq!(machine.metrics.peak_result_wrapper_capacity, 3);
    }
}

#[test]
fn selected_scalar_local_q2_q3_count_env_saved_primary_and_final_clone_until_teardown() {
    for (kind, ty, literal) in PRIMITIVE_CASES {
        for (errors, owners) in [
            ([false, false, false], 2),
            ([true, true, false], 2),
            ([true, false, false], 2),
            ([false, true, false], 2),
            ([true, false, true], 3),
        ] {
            let leaf = |index: usize| {
                if errors[index] {
                    format!("err({})", ["first", "second", "third"][index])
                } else {
                    format!("ok({literal})")
                }
            };
            let replacement = if owners == 2 {
                format!("s: {ty}! = {}\nreturn s", leaf(1))
            } else {
                format!(
                    "s: {ty}! = {}\ntry {{ return s }} finally {{ t: {ty}! = {}\nreturn t }}",
                    leaf(1),
                    leaf(2)
                )
            };
            let source = format!("fn Entry(first: str, second: str, third: str): {ty}! {{ r: {ty}! = {}\ntry {{ return r }} finally {{ {replacement} }}\nreturn ok({literal}) }}", leaf(0));
            for chunk in [1, 7, 256] {
                let mut machine = local_machine(&source, kind);
                let mut host = production_interpreter(&source);
                let (a, first) = string_input("中\0primary", 1024);
                let (b, second) = string_input("é\0replacement", 4096);
                let (c, third) = string_input(PAYLOAD, 2048);
                let buffers = [first, second, third];
                let mut args = vec![a, b, c];
                let mut proof = LocalProof::default();
                machine
                    .start_owned_once("Entry", &mut args, 0, false)
                    .unwrap();
                let mut peak = None;
                if chunk == 1 {
                    pause_at(&mut machine, &mut host, |m| {
                        m.result_wrapper_capacity == owners * 2
                    });
                    let census = observe_local(&machine, &buffers, &mut proof);
                    assert_eq!((census.env.len(), census.live.len()), (owners, owners * 2));
                    assert_eq!(machine.finally.frames.len(), owners - 1);
                    let env_candidate = machine
                        .values
                        .as_slice()
                        .last()
                        .and_then(|value| local_metadata(value, &buffers))
                        .unwrap()
                        .owner;
                    assert!(census.live.contains(&env_candidate));
                    let name = if owners == 2 { "s" } else { "t" };
                    let resident = machine
                        .calls
                        .as_slice()
                        .last()
                        .unwrap()
                        .env
                        .with_value(name, Span::default(), |value| {
                            Ok(local_metadata(value, &buffers))
                        })
                        .unwrap()
                        .unwrap();
                    assert_distinct_local_clone(checked_local_metadata(resident), env_candidate);
                    check_primitive_live_budget(&mut machine);
                    peak = Some((env_candidate, local_error_totals(&census.live)));
                }
                let result =
                    finish_local(&mut machine, &mut host, chunk, &buffers, &mut proof).unwrap();
                let actual = checked_local_metadata(local_metadata(&result, &buffers).unwrap());
                if errors[owners - 1] {
                    assert_eq!(outcome_buffer(&result, &buffers).0, owners - 1);
                } else {
                    assert_eq!(actual.leaf, expected_primitive(kind));
                }
                assert_eq!(
                    (
                        machine.metrics.peak_result_wrapper_capacity,
                        machine.metrics.peak_result_wrapper_bytes,
                        machine.metrics.entered_finally_frames,
                        machine.metrics.retired_finally_frames
                    ),
                    (
                        owners * 2,
                        owners * 2 * std::mem::size_of::<Value>(),
                        owners - 1,
                        owners - 1
                    )
                );
                if let Some((candidate, errors)) = peak {
                    assert_eq!(
                        actual, candidate,
                        "the final real local clone wins with its full Box/Map snapshot"
                    );
                    assert_eq!(proof.peak_env, owners);
                    assert_eq!(
                        (
                            machine.metrics.peak_error_leaf_capacity,
                            machine.metrics.peak_error_leaf_bytes
                        ),
                        errors
                    );
                }
                assert_eq!(
                    machine.metrics.peak_string_payload_bytes,
                    buffers.iter().map(|buffer| buffer.capacity).sum::<usize>()
                );
            }
        }
    }
}

fn retire_s7_task(task: crate::runtime::task::TaskHandle) {
    use crate::runtime::task::{RetirementBatchPoll, TaskRetirementBatch};
    let context = CancellationContext::new(TerminationReason::Cancelled);
    let witness = task.retirement_witness();
    drop(task);
    let mut batch = TaskRetirementBatch::from_reserved(vec![witness], context);
    let end = Instant::now() + Duration::from_secs(3);
    loop {
        match batch.poll(Instant::now()) {
            RetirementBatchPoll::Retired => break,
            RetirementBatchPoll::Pending { .. } => {
                assert!(Instant::now() < end);
                std::thread::yield_now();
            }
            RetirementBatchPoll::TimedOut(context) => {
                panic!("real S7 task retirement missed its original deadline: {context:?}")
            }
        }
    }
}

fn assert_poison_simple_contract(env: &mut Env, name: &str, selected_taskless: bool) {
    let span = Span::default();
    let count = env.current_scope_simple_task_count(span);
    // Zero capacity proves the selected taskless cleanup does not manufacture
    // a proxy, reserve traversal storage, or silently invoke general collection.
    let mut proxies = Vec::new();
    let collected = env.collect_current_scope_simple_tasks_reserved(&mut proxies, span);
    if selected_taskless {
        assert_eq!(count.unwrap(), 0);
        collected.unwrap();
    } else {
        assert!(count.unwrap_err().message.contains("poisoned"));
        assert!(collected.unwrap_err().message.contains("poisoned"));
    }
    assert_eq!((proxies.len(), proxies.capacity()), (0, 0));
    // Poison recovery is limited to strict taskless cleanup metadata. These
    // original ordinary APIs and the general collector must still reject it.
    assert!(env
        .get(name, span)
        .unwrap_err()
        .message
        .contains("poisoned"));
    assert!(env
        .get_owning(name, span)
        .unwrap_err()
        .message
        .contains("poisoned"));
    assert!(env
        .with_value(name, span, |_| Ok(()))
        .unwrap_err()
        .message
        .contains("poisoned"));
    assert!(env
        .assign(name, Value::Null, span)
        .unwrap_err()
        .message
        .contains("poisoned"));
    assert!(env
        .assign_owned(name, Value::Null, span)
        .unwrap_err()
        .message
        .contains("poisoned"));
    assert!(env
        .current_scope_owned_tasks(span)
        .unwrap_err()
        .message
        .contains("poisoned"));
}

fn assert_nonselected_poison_simple_value(value: Value) {
    let span = Span::default();
    let mut env = Env::new();
    env.define_owned("boundary".into(), value, true, span)
        .unwrap();
    let metadata = |env: &Env| {
        env.fold_owned_values(None, |_, value| {
            // Copy representation metadata only; neither snapshot clones nor
            // diagnostic Task handles can add a second physical scope owner.
            Some(match value {
                Value::Int(value) => (1, *value as usize, 0, 0),
                Value::Float(value) => (2, value.to_bits() as usize, 0, 0),
                Value::Bool(value) => (3, usize::from(*value), 0, 0),
                Value::Null => (4, 0, 0, 0),
                Value::String(value) => (5, value.as_ptr() as usize, value.capacity(), value.len()),
                Value::Task(task) => (6, task.id() as usize, 0, 0),
                Value::Result { ok, value } => match value.as_ref() {
                    Value::Task(task) => (
                        7 + usize::from(*ok),
                        value.as_ref() as *const Value as usize,
                        task.id() as usize,
                        0,
                    ),
                    Value::Object(fields) => (
                        9 + usize::from(*ok),
                        value.as_ref() as *const Value as usize,
                        fields.capacity(),
                        fields.len(),
                    ),
                    Value::String(text) => (
                        11 + usize::from(*ok),
                        value.as_ref() as *const Value as usize,
                        text.as_ptr() as usize,
                        text.capacity(),
                    ),
                    _ => panic!("the explicit private boundary fixtures have known shapes"),
                },
                _ => panic!("the explicit private boundary fixtures have known shapes"),
            })
        })
    };
    let original = metadata(&env);
    let native = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        env.with_value::<()>("boundary", span, |_| {
            panic!("actual nonselected cleanup binding poison")
        })
    }));
    assert_eq!(
        native.unwrap_err().downcast_ref::<&str>().copied(),
        Some("actual nonselected cleanup binding poison")
    );
    assert_poison_simple_contract(&mut env, "boundary", false);
    assert_eq!(metadata(&env), original);
    // This Env still owns any real Task until this actual physical Drop.
    drop(env);
}

#[test]
fn selected_scalar_local_operation_clone_native_poison_first_cause_tls_and_unknown_fallback() {
    use std::sync::mpsc;
    for (kind, ty, literal) in PRIMITIVE_CASES {
        for error in [false, true] {
            let leaf = if error {
                "err(text)".to_string()
            } else {
                format!("ok({literal})")
            };
            let source = format!("fn Entry(text: str): {ty}! {{ r: {ty}! = {leaf}\nreturn r }}");
            let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
            let (tx, rx) = mpsc::channel();
            let first_source = source.clone();
            let first = runtime
                .spawn(move || {
                    let mut machine = local_machine(&first_source, kind);
                    let mut host = production_interpreter(&first_source);
                    let (text, buffer) = string_input(PAYLOAD, 4096);
                    let mut args = vec![text];
                    machine
                        .start_owned_once("Entry", &mut args, 0, false)
                        .unwrap();
                    pause_at(&mut machine, &mut host, at_local_clone);
                    let original = checked_local_metadata(
                        env_local_metadata(
                            &machine.calls.as_slice().last().unwrap().env,
                            &[buffer],
                        )
                        .unwrap(),
                    );
                    let now = Instant::now();
                    let initial = CancellationContext {
                        reason: TerminationReason::TimedOut,
                        cleanup_deadline: now,
                    };
                    let shorter = CancellationContext {
                        reason: TerminationReason::Cancelled,
                        cleanup_deadline: now - Duration::from_millis(1),
                    };
                    let mut candidate = None;
                    let mut charge = None;
                    let mut context = None;
                    let native = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let _cleanup = CleanupGuard::enter(initial);
                        machine
                            .calls
                            .as_slice()
                            .last()
                            .unwrap()
                            .env
                            .with_value::<()>("r", Span::default(), |source| {
                                let mut observed = AwaitPayloadCharge::default();
                                let (value, fee) = clone_selected_scalar_result(
                                    source,
                                    unlimited_local_clone_policy(),
                                    &mut observed,
                                )?;
                                candidate =
                                    local_metadata(&value, &[buffer]).map(|meta| meta.owner);
                                charge = Some(fee);
                                drop(CleanupGuard::enter(shorter));
                                context = current_cleanup_context();
                                // A genuinely returned clone owns its Box here beneath
                                // the original source lock. Its RAII Drop and cell
                                // poisoning occur on this actual unwind. This is not
                                // a claim to observe the earlier partial Map stage.
                                let _operation_local = value;
                                panic!("actual selected local returned clone abandonment");
                            })
                            .unwrap();
                    }));
                    let original_panic = native
                        .err()
                        .and_then(|panic| {
                            panic.downcast_ref::<&str>().copied().map(|value| {
                                value == "actual selected local returned clone abandonment"
                            })
                        })
                        .unwrap_or(false);
                    let after = checked_local_metadata(
                        env_local_metadata(
                            &machine.calls.as_slice().last().unwrap().env,
                            &[buffer],
                        )
                        .unwrap(),
                    );
                    let original_charge = machine.frame().env.selected_scalar_result_charge();
                    assert_poison_simple_contract(&mut machine.frame().env, "r", true);
                    assert_eq!(
                        checked_local_metadata(
                            env_local_metadata(&machine.frame().env, &[buffer]).unwrap()
                        ),
                        original,
                        "taskless cleanup preserves the original Box, Map and all six String buffers"
                    );
                    assert_eq!(
                        machine.frame().env.selected_scalar_result_charge(),
                        original_charge
                    );
                    let ordinary_read_poisoned = machine
                        .calls
                        .as_slice()
                        .last()
                        .unwrap()
                        .env
                        .with_value("r", Span::default(), |_| Ok(()))
                        .err()
                        .is_some_and(|error| error.message.contains("poisoned"));
                    let ordinary_assign_poisoned = machine
                        .frame()
                        .env
                        .assign_owned("r", Value::Null, Span::default())
                        .err()
                        .is_some_and(|error| error.message.contains("poisoned"));
                    let env_charge = machine
                        .calls
                        .as_slice()
                        .last()
                        .unwrap()
                        .env
                        .selected_scalar_result_charge();
                    drop(machine);
                    drop(host);
                    let tls_clean = current_cleanup_context().is_none()
                        && crate::runtime::task::current_execution_termination().is_none();
                    tx.send((
                        std::thread::current().id(),
                        crate::runtime::task::current_task_id(),
                        original,
                        after,
                        candidate.unwrap(),
                        charge.unwrap(),
                        context.unwrap(),
                        original_panic,
                        ordinary_read_poisoned,
                        ordinary_assign_poisoned,
                        env_charge,
                        tls_clean,
                        shorter.cleanup_deadline,
                    ))
                    .unwrap();
                    Ok(Value::Null)
                })
                .unwrap();
            let native = rx.recv_timeout(Duration::from_secs(3)).unwrap();
            let first_result = first.await_result();
            retire_s7_task(first);
            let (tx, rx) = mpsc::channel();
            let healthy_source = source.clone();
            let healthy = runtime
                .spawn(move || {
                    let mut host = production_interpreter(&healthy_source);
                    let (text, buffer) = string_input(PAYLOAD, 4096);
                    let value = host.call_function("Entry", vec![text], Span::default(), 0)?;
                    let metadata = local_metadata(&value, &[buffer]).unwrap();
                    tx.send((
                        std::thread::current().id(),
                        crate::runtime::task::current_task_id(),
                        metadata.owner,
                        host.call_depth,
                        current_cleanup_context().is_none(),
                        crate::runtime::task::current_execution_termination().is_none(),
                    ))
                    .unwrap();
                    Ok(value)
                })
                .unwrap();
            let healthy_proof = rx.recv_timeout(Duration::from_secs(3)).unwrap();
            let healthy_result = healthy.await_result();
            retire_s7_task(healthy);
            runtime.cancel_all_and_wait(Duration::from_secs(3)).unwrap();
            let retired = runtime.snapshot().unwrap();
            assert_eq!(
                (
                    retired.active_tasks,
                    retired.queued_tasks,
                    retired.registered_tasks,
                    retired.wait_edges
                ),
                (0, 0, 0, 0)
            );
            assert_eq!(first_result.unwrap(), Value::Null);
            assert_eq!(native.0, healthy_proof.0);
            assert_ne!(native.1, 0);
            assert_ne!(native.1, healthy_proof.1);
            assert_eq!(native.2, native.3);
            assert_distinct_local_clone(native.2, native.4);
            assert_eq!(native.5.box_count, 1);
            assert_eq!(native.5.box_bytes, std::mem::size_of::<Value>());
            assert_eq!(
                native.5.error_leaf_count,
                native.4.error.map_or(0, |value| value.count)
            );
            assert_eq!(
                native.5.error_leaf_bytes,
                native.4.error.map_or(0, |value| value.bytes)
            );
            assert_eq!(native.5.count, 1 + native.5.error_leaf_count);
            assert_eq!(
                native.5.bytes,
                std::mem::size_of::<Value>() + native.5.error_leaf_bytes
            );
            assert_eq!(
                (native.6.reason, native.6.cleanup_deadline),
                (TerminationReason::TimedOut, native.12)
            );
            assert!(native.7 && native.8 && native.9 && native.11);
            assert_eq!(native.10.box_count, 1);
            assert_eq!(native.10.box_bytes, std::mem::size_of::<Value>());
            assert_eq!(
                native.10.error_leaf_count,
                native.2.error.map_or(0, |value| value.count)
            );
            assert_eq!(
                native.10.error_leaf_bytes,
                native.2.error.map_or(0, |value| value.bytes)
            );
            assert_eq!(native.10.count, 1 + native.10.error_leaf_count);
            assert_eq!(
                native.10.bytes,
                std::mem::size_of::<Value>() + native.10.error_leaf_bytes
            );
            assert_eq!(healthy_proof.3, 0);
            assert!(healthy_proof.4 && healthy_proof.5);
            let healthy_result = healthy_result.unwrap();
            assert_eq!(
                local_metadata(
                    &healthy_result,
                    &[Buffer {
                        pointer: 0,
                        capacity: 0,
                        text: PAYLOAD
                    }]
                )
                .unwrap()
                .owner,
                healthy_proof.2
            );
        }
        let source = format!(
            "fn Entry(text: str): {ty}! {{ r: {ty}! = ok({literal})\nmarker = 1 / 0\nreturn r }}"
        );
        let mut machine = local_machine(&source, kind);
        let mut host = production_interpreter(&source);
        let (text, buffer) = string_input(PAYLOAD, 4096);
        let mut args = vec![text];
        let mut proof = LocalProof::default();
        machine
            .start_owned_once("Entry", &mut args, 0, false)
            .unwrap();
        pause_at(
            &mut machine,
            &mut host,
            |m| matches!(&m.pending, Some(Err(error)) if error.message == "division by zero"),
        );
        let original = observe_local(&machine, &[buffer], &mut proof).env[0];
        let poison = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            machine
                .calls
                .as_slice()
                .last()
                .unwrap()
                .env
                .with_value::<()>("r", Span::default(), |_| {
                    panic!("actual selected source cell poison")
                })
        }));
        assert!(poison.is_err());
        assert_eq!(
            observe_local(&machine, &[buffer], &mut proof).env[0],
            original
        );
        assert_poison_simple_contract(&mut machine.frame().env, "r", true);
        assert_eq!(
            observe_local(&machine, &[buffer], &mut proof).env[0],
            original
        );
        let error = finish_local(&mut machine, &mut host, 1, &[buffer], &mut proof).unwrap_err();
        assert_eq!(
            error.message, "division by zero",
            "read-only census never replaces the actual first failure"
        );
        for negative in [
            format!("async fn Entry(text: str): {ty}! {{ r: {ty}! = ok({literal})\nreturn r }}"),
            format!("fn Pass(value: {ty}!): {ty}! {{ return value }}\nfn Entry(text: str): {ty}! {{ r: {ty}! = ok({literal})\nreturn Pass(r) }}"),
            format!("fn Entry(text: str): {ty}! {{ r: {ty}! = ok({literal})\nreturn ok(r?) }}"),
            format!("fn Entry(text: str): {ty}! {{ r: {ty}! = ok({literal})\nreturn r.clone() }}"),
            format!("fn Entry(text: str): {ty}! {{ r: {ty}! = ok({literal})\ntry {{ marker = 1 }} catch (e) {{ marker = 2 }}\nreturn r }}"),
        ] {
            let functions = functions(&negative); let entry = &functions["Entry"];
            assert!(matches!(CodeHandle::compile_owned_named(&functions, entry, Limits::default()), Err(AdmissionError::Unsupported(_))), "{negative}");
            if entry.is_async { assert!(matches!(CodeHandle::compile_claimed_async_owned_scalar_result_entry(&functions, entry, Limits::default()), Err(AdmissionError::Unsupported(_)))); }
            let mut host = production_interpreter(&negative); let (text, buffer) = string_input(PAYLOAD, 4096); let mut args = vec![text];
            assert!(try_execute_owned(&mut host, entry, &mut args, 0).is_none()); assert_eq!((host.steps, host.call_depth), (0, 0));
            assert!(matches!(&args[0], Value::String(text) if text.as_ptr() as usize == buffer.pointer && text.capacity() == buffer.capacity));
        }
    }
    // These are private cleanup-classifier contracts, not source-language
    // claims that ordinary Ku code naturally poisons an Env cell. None may
    // acquire the strict selected primitive Result's poison recovery.
    for value in [
        Value::String(PAYLOAD.into()),
        Value::Int(7),
        Value::Float(1.25),
        Value::Bool(true),
        Value::Null,
        Value::Result {
            ok: true,
            value: Box::new(Value::String(PAYLOAD.into())),
        },
    ] {
        assert_nonselected_poison_simple_value(value);
    }
    for fields in [
        HashMap::from([("message".to_string(), Value::String(PAYLOAD.into()))]),
        HashMap::from([
            ("domain".to_string(), Value::String("ku".into())),
            ("code".to_string(), Value::String("err".into())),
            ("message".to_string(), Value::Int(7)),
        ]),
        HashMap::from([
            ("domain".to_string(), Value::String("ku".into())),
            ("code".to_string(), Value::String("err".into())),
            ("message".to_string(), Value::String(PAYLOAD.into())),
            ("extra".to_string(), Value::Bool(true)),
        ]),
    ] {
        assert_nonselected_poison_simple_value(Value::Result {
            ok: false,
            value: Box::new(Value::Object(fields)),
        });
    }
    {
        use crate::runtime::task::{RetirementBatchPoll, TaskRetirementBatch};
        let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
        for result_flag in [None, Some(true), Some(false)] {
            let task = runtime.spawn(|| Ok(Value::Int(7))).unwrap();
            assert_eq!(task.await_result().unwrap(), Value::Int(7));
            let witness = task.retirement_witness();
            let value = Value::Task(task);
            let value = match result_flag {
                None => value,
                Some(ok) => Value::Result {
                    ok,
                    value: Box::new(value),
                },
            };
            // Transfer the sole real Task handle into the fixture's Env. The
            // classifier must reject poison before collecting any proxy.
            assert_nonselected_poison_simple_value(value);
            let context = CancellationContext::new(TerminationReason::Cancelled);
            let mut batch = TaskRetirementBatch::from_reserved(vec![witness], context);
            let end = Instant::now() + Duration::from_secs(3);
            loop {
                match batch.poll(Instant::now()) {
                    RetirementBatchPoll::Retired => break,
                    RetirementBatchPoll::Pending { .. } => {
                        assert!(Instant::now() < end);
                        std::thread::yield_now();
                    }
                    RetirementBatchPoll::TimedOut(context) => {
                        panic!("real poisoned boundary Task missed retirement: {context:?}")
                    }
                }
            }
        }
        runtime.cancel_all_and_wait(Duration::from_secs(3)).unwrap();
        let retired = runtime.snapshot().unwrap();
        assert_eq!(
            (
                retired.active_tasks,
                retired.queued_tasks,
                retired.registered_tasks,
                retired.wait_edges
            ),
            (0, 0, 0, 0)
        );
    }
    let string_local = "fn Entry(text: str): str! { r: str! = ok(text)\nreturn r }";
    let strings = functions(string_local);
    assert!(matches!(
        CodeHandle::compile_owned_named(&strings, &strings["Entry"], Limits::default()),
        Err(AdmissionError::Unsupported(_))
    ));
    let source = "fn Entry(text: str, value: float): float! { r: float! = ok(value)\nr\nr\nr = r\nreturn r }";
    let source_functions = functions(source);
    let mut old = production_interpreter(source);
    let unknown = crate::stdlib::array::eval("first", &[Value::Array(Vec::new())], Span::default())
        .unwrap()
        .unwrap();
    let (text, buffer) = string_input(PAYLOAD, 4096);
    let legacy = old
        .call_function_direct_flow(
            source_functions["Entry"].clone(),
            vec![text, unknown],
            Span::default(),
            0,
        )
        .unwrap()
        .into_call_value(Span::default(), "function")
        .unwrap();
    assert_eq!(
        local_metadata(&legacy, &[buffer]).unwrap().owner.leaf,
        PrimitiveLeaf::Null
    );
    let mut machine = local_machine(source, PrimitiveResultKind::Float);
    let mut host = production_interpreter(source);
    let (text, buffer) = string_input(PAYLOAD, 4096);
    let mut args = vec![text, Value::Null];
    let mut proof = LocalProof::default();
    machine
        .start_owned_once("Entry", &mut args, 0, false)
        .unwrap();
    let result = finish_local(&mut machine, &mut host, 1, &[buffer], &mut proof).unwrap();
    assert_eq!(
        local_metadata(&result, &[buffer]).unwrap().owner.leaf,
        PrimitiveLeaf::Null
    );
    let complete_null = format!("{source}\nfn RequireNull(value): null! {{ if (value == null) {{ return ok(null) }}\nreturn err(\"Unknown local changed\") }}\nfn main(): null! {{ items = []\nunknown = array.first(items)\nactual = Entry(\"{PAYLOAD}\", unknown)?\nRequireNull(actual)?\nreturn ok(null) }}");
    let program = Parser::new(Lexer::new(&complete_null).tokenize().unwrap())
        .parse_program()
        .unwrap();
    crate::checker::Checker::new().check(&program).unwrap();
    crate::cli::run_source("selected-local-unknown-null.ku", &complete_null).unwrap();
    for syntax in ["[1, 2]", "{answer: 7}"] {
        let complete = format!("{source}\nfn main(): null! {{ items = []\nitems = array.push(items, {syntax})\nunknown = array.first(items)\nactual = Entry(\"{PAYLOAD}\", unknown)?\nreturn ok(null) }}");
        let program = Parser::new(Lexer::new(&complete).tokenize().unwrap())
            .parse_program()
            .unwrap();
        crate::checker::Checker::new().check(&program).unwrap();
        let opaque = if syntax.starts_with('[') {
            Value::Array(vec![Value::Int(1), Value::Int(2)])
        } else {
            Value::Object(HashMap::from([("answer".to_string(), Value::Int(7))]))
        };
        let unknown =
            crate::stdlib::array::eval("first", &[Value::Array(vec![opaque])], Span::default())
                .unwrap()
                .unwrap();
        let identity = match &unknown {
            Value::Array(values) => (values.as_ptr() as usize, values.capacity()),
            Value::Object(fields) => (
                fields.get_key_value("answer").unwrap().0 as *const String as usize,
                fields.capacity(),
            ),
            _ => unreachable!(),
        };
        let (text, buffer) = string_input(PAYLOAD, 4096);
        let mut args = vec![text, unknown];
        let mut host = production_interpreter(source);
        assert!(try_execute_owned(&mut host, &source_functions["Entry"], &mut args, 0).is_none());
        assert_eq!((host.steps, host.call_depth), (0, 0));
        assert!(
            matches!(&args[0], Value::String(text) if text.as_ptr() as usize == buffer.pointer && text.capacity() == buffer.capacity)
        );
        let after = match &args[1] {
            Value::Array(values) => (values.as_ptr() as usize, values.capacity()),
            Value::Object(fields) => (
                fields.get_key_value("answer").unwrap().0 as *const String as usize,
                fields.capacity(),
            ),
            _ => unreachable!(),
        };
        assert_eq!(after, identity);
        let legacy = host
            .call_function("Entry", args, Span::default(), 0)
            .unwrap();
        let Value::Result { ok: true, value } = legacy else {
            panic!("whole legacy keeps legal opaque Unknown capability")
        };
        assert!(matches!(value.as_ref(), Value::Array(_) | Value::Object(_)));
        crate::cli::run_source("selected-local-unknown-opaque.ku", &complete).unwrap();
    }
}
