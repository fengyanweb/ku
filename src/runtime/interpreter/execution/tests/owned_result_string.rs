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
        assert!(
            buffers.iter().any(|buffer| buffer.text == message.as_str()),
            "the error message comes from one real input"
        );
        // The original operand can already be retired. Its historical input
        // pointer cannot establish clone separation for this live descendant;
        // source and candidate are compared while both physically coexist.
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
    std::thread::scope(|scope| {
        for (kind, ty, literal) in PRIMITIVE_CASES {
            for error in [false, true] {
                scope.spawn(move || {
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
                });
            }
        }
    });
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
            if entry.is_async {
                // Charged async locals are now admitted by S8. The same tree
                // must still reject the synchronous and uncharged roles.
                let charged = CodeHandle::compile_claimed_async_owned_scalar_result_entry(&functions, entry, Limits::default()).unwrap();
                let function = charged.function(charged.find_function("Entry").unwrap());
                assert!(function.is_async);
                assert_eq!(function.owned_scalar_result, Some(kind));
                assert!(matches!(CodeHandle::compile_claimed_async_entry(&functions, entry, Limits::default()), Err(AdmissionError::Unsupported(_))));
            }
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
    let string_local = "fn Entry(text: str, unused: str): str! { r: str! = ok(text)\nreturn r }";
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct S9String {
    pointer: usize,
    len: usize,
    capacity: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct S9Owner {
    boxed: usize,
    string: Option<S9String>,
    error: Option<ErrorPayload>,
}

fn s9_owner(value: &Value, text: &'static str) -> Option<S9Owner> {
    let Value::Result { ok, value: leaf } = value else {
        return None;
    };
    let boxed = leaf.as_ref() as *const Value as usize;
    if *ok {
        let Value::String(actual) = leaf.as_ref() else {
            panic!("the selected String success retains its real String leaf");
        };
        assert_eq!(actual, text);
        Some(S9Owner {
            boxed,
            string: Some(S9String {
                pointer: actual.as_ptr() as usize,
                len: actual.len(),
                capacity: actual.capacity(),
            }),
            error: None,
        })
    } else {
        let error = error_payload(value).expect("the real selected ErrorObject");
        let Value::Object(fields) = leaf.as_ref() else {
            unreachable!()
        };
        assert!(matches!(fields.get("domain"), Some(Value::String(value)) if value == "ku"));
        assert!(matches!(fields.get("code"), Some(Value::String(value)) if value == "err"));
        assert!(matches!(fields.get("message"), Some(Value::String(value)) if value == text));
        Some(S9Owner {
            boxed,
            string: None,
            error: Some(error),
        })
    }
}

struct S9Census {
    env: Vec<S9Owner>,
    owners: Vec<S9Owner>,
    original_seen: bool,
    clone_bytes: usize,
}

// Fixture-owned metadata is allocated before any BindingCell lock. The fold
// copies real addresses/lengths/capacities and never clones or takes a Value.
// Besides Result owners, bare Strings in arguments/operands must be counted.
fn s9_census(
    machine: &Machine,
    input: Buffer,
    original_live: bool,
    payload_text: &'static str,
    task_errors: bool,
    allow_await_ticket: bool,
    strings: &mut Vec<S9String>,
) -> S9Census {
    if !allow_await_ticket {
        assert!(machine.awaiting.is_none(), "the S9 slice cannot await");
        assert_eq!(
            machine.await_payload_snapshot,
            AwaitPayloadCharge::default()
        );
        assert_eq!(machine.await_string_payload_snapshot.bytes, 0);
        assert_eq!(machine.await_payload_escrow, AwaitPayloadCharge::default());
        assert_eq!(machine.await_string_payload_escrow.bytes, 0);
    }
    let staged = machine
        .work
        .as_slice()
        .iter()
        .filter_map(|work| match work {
            Work::ArgumentsExit(frame) => Some(frame.staged.len()),
            _ => None,
        })
        .sum::<usize>();
    let capacity = machine.calls.len() * 8
        + staged
        + machine.values.len()
        + machine.exits.len() * 8
        + machine.finally.frames.len() * 4
        + 128;
    // These fixed fixtures have at most two local Result cells, two saved
    // Q primaries and one transferring outcome. Deep parents move their
    // only bare String down before binding r; they install no keepers.
    // This independent fixture bound is not a runtime resource limit. The
    // spare-backing assertion still fails if a future fixture exceeds it.
    let mut env = Vec::with_capacity(8);
    let mut owners = Vec::with_capacity(8);
    strings.clear();
    // Reserve the complete independent String upper bound outside locks;
    // reuse its backing without retaining any previous live-owner record.
    strings.reserve(capacity);
    let mut record = |value: &Value, in_env: bool| {
        let owner = s11_measured_owner(value, payload_text, task_errors);
        if let Some(owner) = owner {
            assert!(
                owners.len() < owners.capacity(),
                "fixture metadata has spare backing"
            );
            owners.push(owner);
            if in_env {
                assert!(env.len() < env.capacity());
                env.push(owner);
            }
        }
        let actual = match value {
            Value::String(actual) => Some(actual),
            Value::Result { ok: true, value } => match value.as_ref() {
                Value::String(actual) => Some(actual),
                _ => None,
            },
            _ => None,
        };
        if let Some(actual) = actual {
            assert!(actual == input.text || actual == payload_text);
            assert!(strings.len() < strings.capacity());
            strings.push(S9String {
                pointer: actual.as_ptr() as usize,
                len: actual.len(),
                capacity: actual.capacity(),
            });
        }
    };
    for frame in machine.calls.as_slice() {
        let mut has_result = false;
        let mut has_string_success = false;
        let mut has_string = false;
        frame.env.fold_owned_values((), |(), value| {
            has_result |= matches!(value, Value::Result { .. });
            has_string_success |= matches!(value, Value::Result { ok: true, value }
                if matches!(value.as_ref(), Value::String(_)));
            has_string |= matches!(value, Value::String(_))
                || matches!(value, Value::Result { ok: true, value }
                    if matches!(value.as_ref(), Value::String(_)));
            record(value, true);
        });
        if has_string {
            assert!(frame.may_have_string_owner_bindings);
        }
        if has_result {
            assert!(frame.has_selected_result_bindings);
        }
        if has_string_success {
            assert!(frame.has_selected_string_result_bindings);
        }
    }
    if let Some(args) = &machine.entry_args {
        for value in args {
            record(value, false);
        }
    }
    for value in machine.values.as_slice() {
        record(value, false);
    }
    for work in machine.work.as_slice() {
        if let Work::ArgumentsExit(frame) = work {
            for value in &frame.staged {
                record(value, false);
            }
        }
    }
    let mut record_flow = |result: &KuResult<Flow>| {
        if let Ok(Flow::Return { value, .. } | Flow::Fail { value, .. }) = result {
            record(value, false);
        }
    };
    if let Some(result) = &machine.pending {
        record_flow(result);
    }
    for exit in machine.exits.as_slice() {
        if let Some(result) = exit.state.outcome() {
            record_flow(result);
        }
    }
    for frame in machine.finally.frames.as_slice() {
        if let Some(result) = &frame.primary {
            record_flow(result);
        }
    }
    for exit in machine.exits.as_slice() {
        if let ExitCompletion::Arguments { frame, .. } = &exit.completion {
            for value in &frame.staged {
                record(value, false);
            }
        }
    }
    let physical_owners = owners.len();
    let physical_strings = strings.len();
    let ticket = machine.awaiting.as_ref().and_then(|frame| {
        frame
            .ticket
            .test_outcome()
            .and_then(|result| result.as_ref().ok())
            .and_then(|value| s11_measured_owner(value, payload_text, task_errors))
    });
    let mut ticket_string_bytes = 0;
    if let Some(frame) = &machine.awaiting {
        // The S11 callers supply actual Ready metadata; the existing S9/S10
        // fixtures never reach this branch. This does not revive input credit.
        let measured = ticket.map_or_else(AwaitPayloadCharge::default, s11_box_error_charge);
        assert_eq!(frame.ticket.retained_payload_charge(), measured);
        ticket_string_bytes = ticket
            .and_then(|owner| owner.string)
            .map_or(0, |text| text.capacity);
        assert_eq!(
            frame.ticket.retained_string_payload_charge().bytes,
            ticket_string_bytes,
            "the independent local-await policy charges all real success capacity"
        );
        assert_eq!(machine.await_payload_escrow, AwaitPayloadCharge::default());
        assert_eq!(
            machine.await_string_payload_escrow.bytes, 0,
            "operation-local escrow is tested inside the actual operation, not stable census"
        );
    }
    if let Some(ticket) = ticket {
        owners.push(ticket);
        if let Some(text) = ticket.string {
            strings.push(text);
        }
    }
    let mut boxes = std::collections::HashSet::with_capacity(owners.len());
    for owner in &owners {
        assert!(boxes.insert(owner.boxed), "one real owner per Result Box");
    }
    let original_seen = strings.iter().take(physical_strings).any(|actual| {
        original_live
            && input.capacity != 0
            && actual.pointer == input.pointer
            && actual.capacity == input.capacity
    });
    let physical_clone_bytes: usize = strings
        .iter()
        .take(physical_strings)
        .filter(|actual| {
            !(original_live
                && input.capacity != 0
                && actual.pointer == input.pointer
                && actual.capacity == input.capacity)
        })
        .map(|actual| actual.capacity)
        .sum();
    let clone_bytes = physical_clone_bytes + ticket_string_bytes;
    let errors: usize = owners
        .iter()
        .take(physical_owners)
        .filter_map(|owner| owner.error)
        .map(|error| error.count)
        .sum();
    let error_bytes: usize = owners
        .iter()
        .take(physical_owners)
        .filter_map(|owner| owner.error)
        .map(|error| error.bytes)
        .sum();
    let stable_boxes = physical_owners + machine.await_payload_snapshot.box_count;
    let stable_errors = errors + machine.await_payload_snapshot.error_leaf_count;
    let stable_error_bytes = error_bytes + machine.await_payload_snapshot.error_leaf_bytes;
    let stable_clones = physical_clone_bytes + machine.await_string_payload_snapshot.bytes;
    assert_eq!(machine.result_wrapper_capacity, stable_boxes);
    assert_eq!(
        machine.result_wrapper_bytes,
        stable_boxes * std::mem::size_of::<Value>()
    );
    assert_eq!(machine.error_leaf_capacity, stable_errors);
    assert_eq!(machine.error_leaf_bytes, stable_error_bytes);
    assert_eq!(
        machine.metrics.retained_result_wrapper_capacity,
        stable_boxes
    );
    assert_eq!(
        machine.metrics.retained_result_wrapper_bytes,
        machine.result_wrapper_bytes
    );
    assert_eq!(machine.metrics.retained_error_leaf_capacity, stable_errors);
    assert_eq!(
        machine.metrics.retained_error_leaf_bytes,
        stable_error_bytes
    );
    assert_eq!(machine.cloned_string_payload_bytes, stable_clones);
    assert_eq!(
        machine.metrics.retained_cloned_string_payload_bytes,
        stable_clones
    );
    assert_eq!(
        machine.string_payload_bytes, input.capacity,
        "unused input reservation is never refunded"
    );
    // The original input metric is finalized at finish/drop. The live input
    // reservation above, unlike that cached metric, is authoritative here.
    let original = machine
        .original_string
        .expect("selected owned entry installs real origin metadata");
    assert_eq!(
        (original.pointer, original.capacity),
        (input.pointer, input.capacity)
    );
    assert_eq!(
        original.still_live, original_seen,
        "absence from eligible bare/success String owners permanently ends the original credit"
    );
    S9Census {
        env,
        owners,
        original_seen,
        clone_bytes,
    }
}

struct S9Proof {
    original_live: bool,
    payload_text: &'static str,
    task_errors: bool,
    allow_await_ticket: bool,
    live: Vec<S9Owner>,
    string_scratch: Vec<S9String>,
    final_clone: Option<S9Owner>,
    peak_live: usize,
    peak_clone_bytes: usize,
}

impl S9Proof {
    fn new(input: Buffer) -> Self {
        Self {
            original_live: input.capacity != 0,
            payload_text: input.text,
            task_errors: false,
            allow_await_ticket: false,
            live: Vec::new(),
            string_scratch: Vec::new(),
            final_clone: None,
            peak_live: 0,
            peak_clone_bytes: 0,
        }
    }
}

fn s9_observe(machine: &Machine, input: Buffer, proof: &mut S9Proof) -> S9Census {
    let actual = s9_census(
        machine,
        input,
        proof.original_live,
        proof.payload_text,
        proof.task_errors,
        proof.allow_await_ticket,
        &mut proof.string_scratch,
    );
    proof.original_live &= actual.original_seen;
    proof.live.clone_from(&actual.owners); // Copy-only metadata; no Value clone.
    proof.peak_live = proof.peak_live.max(actual.owners.len());
    proof.peak_clone_bytes = proof.peak_clone_bytes.max(actual.clone_bytes);
    actual
}

fn s9_machine(source: &str) -> Machine {
    let source_functions = functions(source);
    let entry = &source_functions["Entry"];
    assert!(code::eligible_owned_named(
        &source_functions,
        entry,
        Limits::default()
    ));
    let code =
        CodeHandle::compile_owned_named(&source_functions, entry, Limits::default()).unwrap();
    assert!(code.has_actual_string_result_locals());
    Machine::from_code(code, Limits::default(), 0)
}

fn s9_at_clone(machine: &Machine) -> bool {
    machine.pending.is_none()
        && matches!(
            machine.work.as_slice().last(),
            Some(Work::CloneSelectedStringResult(_, _))
        )
}

fn s9_explicit_clone_ticks(
    machine: &mut Machine,
    host: &mut Interpreter,
    input: Buffer,
    proof: &mut S9Proof,
) -> bool {
    if machine.pending.is_some() {
        return false;
    }
    let Some(Work::Eval(id)) = machine.work.as_slice().last() else {
        return false;
    };
    let code::ExprCode::SelectedStringResultCloneCall(target) = machine.code.expression(*id).kind
    else {
        return false;
    };
    let steps = host.steps;
    let kernel_steps = machine.metrics.steps;
    let before = s9_observe(machine, input, proof);
    for delta in 1..=2 {
        assert!(matches!(
            machine
                .dispatch_chunk(NonZeroUsize::new(1).unwrap(), host)
                .unwrap(),
            ChunkProgress::More { popped: 1 }
        ));
        assert_eq!(host.steps, steps + delta);
        assert_eq!(machine.metrics.steps, kernel_steps + delta);
        let after = s9_observe(machine, input, proof);
        assert_eq!(
            after.owners, before.owners,
            "ticks do not copy or move owners"
        );
        if delta == 1 {
            assert!(
                matches!(machine.work.as_slice().last(), Some(Work::Eval(id)) if *id == target)
            );
        }
    }
    assert!(
        s9_at_clone(machine),
        "the sole real clone follows both ticks"
    );
    true
}

fn s9_assert_distinct(source: S9Owner, cloned: S9Owner) {
    assert_ne!(
        source.boxed, cloned.boxed,
        "source and clone Boxes are simultaneously alive"
    );
    match (source.string, cloned.string, source.error, cloned.error) {
        (Some(a), Some(b), None, None) => {
            assert_eq!(a.len, b.len);
            if a.len != 0 {
                assert_ne!(a.pointer, b.pointer);
            }
        }
        (None, None, Some(a), Some(b)) => {
            for index in 0..3 {
                assert_ne!(a.entries[index], b.entries[index]);
            }
            for index in 0..6 {
                if a.buffers[index].1 != 0 && b.buffers[index].1 != 0 {
                    assert_ne!(a.buffers[index].0, b.buffers[index].0);
                }
            }
        }
        _ => panic!("a clone preserves its actual success/failure shape"),
    }
}

// At chunk one, copy the actual source metadata before the sole clone Work.
// The next census sees both real owners alive. No dead-address history or
// outgoing outcome is used to manufacture an earlier ownership witness.
fn s9_clone_step(
    machine: &mut Machine,
    host: &mut Interpreter,
    input: Buffer,
    proof: &mut S9Proof,
) {
    let before = s9_observe(machine, input, proof);
    let Some(Work::CloneSelectedStringResult(name, span)) = machine.work.as_slice().last() else {
        panic!("the real clone continuation");
    };
    let source = machine
        .calls
        .as_slice()
        .last()
        .unwrap()
        .env
        .with_value(
            machine.code.symbol(*name),
            machine.code.span(*span),
            |value| Ok(s9_owner(value, input.text).unwrap()),
        )
        .unwrap();
    assert!(before.env.contains(&source));
    assert!(matches!(
        machine
            .dispatch_chunk(NonZeroUsize::new(1).unwrap(), host)
            .unwrap(),
        ChunkProgress::More { popped: 1 }
    ));
    let after = s9_observe(machine, input, proof);
    assert!(
        after.env.contains(&source),
        "the complete source metadata is unchanged under clone"
    );
    let cloned = s9_owner(machine.values.as_slice().last().unwrap(), input.text).unwrap();
    assert!(after.owners.contains(&cloned));
    s9_assert_distinct(source, cloned);
    proof.final_clone = Some(cloned);
}

fn s9_budget(machine: &mut Machine, census: &S9Census, input: Buffer) {
    let saved = machine.value_limits;
    let count = machine.values.capacity()
        + machine.argument_capacity
        + machine.exit_handle_capacity
        + machine.retirement_capacity
        + machine.lease_capacity
        + machine.lease_root_bytes / std::mem::size_of::<Value>()
        + census.owners.len()
        + census
            .owners
            .iter()
            .filter_map(|owner| owner.error)
            .map(|error| error.count)
            .sum::<usize>();
    let bytes = machine.values.charged_storage_bytes()
        + machine.argument_bytes
        + machine.exit_handle_bytes
        + machine.retirement_bytes
        + machine.lease_bytes
        + machine.lease_root_bytes
        + input.capacity
        + census.clone_bytes
        + census.owners.len() * std::mem::size_of::<Value>()
        + census
            .owners
            .iter()
            .filter_map(|owner| owner.error)
            .map(|error| error.bytes)
            .sum::<usize>();
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

fn s9_drained(machine: &Machine) {
    errors_drained(machine);
    assert!(machine.original_string.is_none());
    assert_eq!(machine.cloned_string_payload_bytes, 0);
    assert_eq!(machine.metrics.retained_cloned_string_payload_bytes, 0);
}

fn s9_finish(
    machine: &mut Machine,
    host: &mut Interpreter,
    chunk: usize,
    input: Buffer,
    proof: &mut S9Proof,
) -> KuResult<Value> {
    for _ in 0..100_000 {
        if chunk == 1 && s9_at_clone(machine) {
            s9_clone_step(machine, host, input, proof);
            continue;
        }
        match machine
            .dispatch_chunk(NonZeroUsize::new(chunk).unwrap(), host)
            .unwrap()
        {
            ChunkProgress::More { popped } => {
                assert_eq!(popped, chunk);
                s9_observe(machine, input, proof);
            }
            ChunkProgress::Waiting { .. } => {
                panic!("synchronous selected String locals cannot wait")
            }
            ChunkProgress::Finished { popped, result } => {
                assert!(popped <= chunk);
                let result =
                    result.and_then(|flow| flow.into_call_value(Span::default(), "function"));
                if let Ok(value) = &result {
                    let actual = s9_owner(value, input.text).unwrap();
                    if chunk == 1 {
                        assert!(
                            proof.live.contains(&actual),
                            "final owner was observed before teardown"
                        );
                        assert_eq!(
                            proof.final_clone,
                            Some(actual),
                            "the real final local read wins"
                        );
                    }
                }
                s9_drained(machine);
                return result;
            }
        }
    }
    panic!("the finite S9 fixture did not finish");
}

#[test]
fn selected_string_locals_clone_source_and_actual_success_or_error_owners_across_chunks() {
    for error in [false, true] {
        for helper in [false, true] {
            let leaf = if error { "err(text)" } else { "ok(text)" };
            let prefix = if helper {
                format!("fn Leaf(text: str, marker: int): str! {{ return {leaf} }}\n")
            } else {
                String::new()
            };
            let rhs = if helper { "Leaf(text, 7)" } else { leaf };
            let source = format!(
                "{prefix}fn Entry(text: str): str! {{ r: str! = {rhs}\nr\nr\nr = r\nreturn r }}"
            );
            for chunk in [1, 7, 256] {
                let mut machine = s9_machine(&source);
                let mut host = production_interpreter(&source);
                let (value, buffer) = string_input(PAYLOAD, 4096);
                let mut args = vec![value];
                let mut proof = S9Proof::new(buffer);
                machine
                    .start_owned_once_with_hooks("Entry", &mut args, 0, false, &host)
                    .unwrap();
                assert!(args.is_empty());
                s9_observe(&machine, buffer, &mut proof);
                if chunk == 1 {
                    pause_at(&mut machine, &mut host, s9_at_clone);
                    let before = s9_observe(&machine, buffer, &mut proof);
                    assert_eq!(before.env.len(), 1);
                    assert_eq!(before.clone_bytes, 0);
                    s9_budget(&mut machine, &before, buffer);
                    s9_clone_step(&mut machine, &mut host, buffer, &mut proof);
                    let after = s9_observe(&machine, buffer, &mut proof);
                    assert_eq!((after.env.len(), after.owners.len()), (1, 2));
                    s9_budget(&mut machine, &after, buffer);
                }
                let value = s9_finish(&mut machine, &mut host, chunk, buffer, &mut proof).unwrap();
                let result = s9_owner(&value, PAYLOAD).unwrap();
                assert_eq!(result.error.is_some(), error);
                if let Some(actual) = result.string {
                    assert_eq!(actual.len, PAYLOAD.len());
                    assert!(machine.metrics.peak_cloned_string_payload_bytes >= actual.capacity);
                } else {
                    assert_eq!(machine.metrics.peak_cloned_string_payload_bytes, 0);
                }
                assert_eq!(machine.metrics.peak_string_payload_bytes, buffer.capacity);
                if chunk == 1 {
                    assert_eq!(
                        machine.metrics.peak_cloned_string_payload_bytes,
                        proof.peak_clone_bytes
                    );
                    assert_eq!(
                        machine.metrics.peak_result_wrapper_capacity,
                        proof.peak_live
                    );
                }
                assert_eq!(host.call_depth, 0);
            }
        }
    }
}

#[test]
fn selected_string_local_clone_minimum_rejection_preserves_source_and_original_reservation() {
    const SOURCE: &str = "fn Entry(text: str): str! { r: str! = ok(text)\nr\nreturn r }";
    for count_rejection in [false, true] {
        let mut machine = s9_machine(SOURCE);
        let mut host = production_interpreter(SOURCE);
        let (value, buffer) = string_input(PAYLOAD, 4096);
        let mut args = vec![value];
        let mut proof = S9Proof::new(buffer);
        machine
            .start_owned_once_with_hooks("Entry", &mut args, 0, false, &host)
            .unwrap();
        pause_at(&mut machine, &mut host, s9_at_clone);
        let before = s9_observe(&machine, buffer, &mut proof);
        let source = before.env[0];
        machine.reserve_values(1, Span::default()).unwrap();
        let saved = machine.value_limits;
        if count_rejection {
            machine.value_limits.max_frames = live_value_count(&machine);
        } else {
            // Exactly enough for a new Box but no independent nonempty buffer.
            machine.value_limits.max_storage_bytes = live_value_bytes(&machine)
                + machine.cloned_string_payload_bytes
                + std::mem::size_of::<Value>();
        }
        machine
            .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut host)
            .unwrap();
        assert!(
            matches!(&machine.pending, Some(Err(error)) if error.message.contains(
            if count_rejection { "FrameLimit" } else { "StorageLimit" }))
        );
        let after = s9_observe(&machine, buffer, &mut proof);
        assert_eq!(after.env, before.env);
        assert!(after.owners.contains(&source));
        assert_eq!(machine.metrics.peak_result_wrapper_capacity, 1);
        assert_eq!(
            machine.metrics.peak_cloned_string_payload_bytes, 0,
            "minimum rejection allocates no new buffer"
        );
        machine.value_limits = saved;
        s9_finish(&mut machine, &mut host, 1, buffer, &mut proof).unwrap_err();
        machine.prepare_next_run().unwrap();
        let (value, buffer) = string_input(PAYLOAD, 4096);
        let mut args = vec![value];
        let mut proof = S9Proof::new(buffer);
        machine
            .start_owned_once_with_hooks("Entry", &mut args, 0, false, &host)
            .unwrap();
        s9_finish(&mut machine, &mut host, 1, buffer, &mut proof).unwrap();
    }
}

fn s9_deep_finish(
    machine: &mut Machine,
    host: &mut Interpreter,
    input: Buffer,
    proof: &mut S9Proof,
    budget_chunk: usize,
) -> KuResult<Value> {
    let mut deepest = false;
    for _ in 0..100_000 {
        if machine.calls.len() == 512 && !deepest {
            // One input moves down the 512 real call frames. No artificial
            // success keeper is installed in an outer Env to multiply owners.
            let actual = s9_observe(machine, input, proof);
            assert!(actual.env.len() <= 1);
            s9_budget(machine, &actual, input);
            deepest = true;
        }
        if deepest && machine.calls.len() <= 32 {
            // Establish an independent continuous final witness; previous
            // large chunks never assert identity across destroyed addresses.
            proof.live.clear();
            proof.final_clone = None;
            return s9_finish(machine, host, 1, input, proof);
        }
        let chunk = if deepest {
            budget_chunk
        } else {
            // Each Work/exit enters at most one call. This exposes the exact
            // first depth-512 checkpoint without loosening its 512 limit.
            budget_chunk.min((512 - machine.calls.len()).max(1))
        };
        match machine
            .dispatch_chunk(NonZeroUsize::new(chunk).unwrap(), host)
            .unwrap()
        {
            ChunkProgress::More { popped } => {
                assert_eq!(popped, chunk);
                if deepest || machine.calls.len() % 64 == 0 {
                    s9_observe(machine, input, proof);
                }
            }
            ChunkProgress::Waiting { .. } => panic!("S9 has no await"),
            ChunkProgress::Finished { .. } => {
                panic!("the exact deep checkpoint and final owner must be exposed")
            }
        }
    }
    panic!("the finite S9 deep source did not finish");
}

#[test]
fn selected_string_local_self_mutual_and_fresh_graphs_keep_real_512_first_513_and_reuse() {
    // The success/error fixtures share no Machine, Env, clock or TLS state.
    // Keep each complete proof on its construction thread, then join both;
    // this changes fixture scheduling only, with default stack sizes.
    std::thread::scope(|scope| {
        for error in [false, true] {
            scope.spawn(move || {
        let leaf = if error { "err(text)" } else { "ok(text)" };
        let body = format!("if (n <= 0) {{ r: str! = {leaf}\nr\nr = r\nreturn r }}\nr: str! = NEXT(text, n - 1)\nr\nr = r\nreturn r");
        let self_source = format!(
            "fn Entry(text: str, n: int): str! {{ {} }}",
            body.replace("NEXT", "Entry")
        );
        let mutual_source = format!("fn Entry(text: str, n: int): str! {{ {} }}\nfn Other(text: str, n: int): str! {{ {} }}",
            body.replace("NEXT", "Other"), body.replace("NEXT", "Entry"));
        let fresh_source = format!("fn Entry(text: str, n: int): str! {{ r: str! = Bridge(text, n)\nr\nr = r\nreturn r }}\nfn Bridge(text: str, n: int): str! {{ {} }}", body.replace("NEXT", "Bridge"));
        // Distribute deep chunks across the three actual call graphs.
        // The shallow direct/helper clone group covers all chunk sizes
        // and both success/failure outcomes.
        for (source, extra, deep_chunk) in [
            (&self_source, 0, 1),
            (&mutual_source, 0, 7),
            (&fresh_source, 1, 256),
        ] {
            let chunk = deep_chunk;
            let mut machine = s9_machine(source);
            let mut host = production_interpreter(source);
            for n in [511 - extra, 512 - extra, 0] {
                if machine.state != MachineState::Ready {
                    machine.prepare_next_run().unwrap();
                }
                let entered = machine.metrics.entered_calls;
                let retired = machine.metrics.retired_calls;
                let (value, buffer) = string_input(PAYLOAD, 4096);
                let mut args = vec![value, Value::Int(n)];
                let mut proof = S9Proof::new(buffer);
                machine
                    .start_owned_once_with_hooks("Entry", &mut args, 0, false, &host)
                    .unwrap();
                let result = if n == 0 {
                    s9_finish(&mut machine, &mut host, chunk, buffer, &mut proof)
                } else {
                    s9_deep_finish(&mut machine, &mut host, buffer, &mut proof, chunk)
                };
                if n == 512 - extra {
                    assert_eq!(
                        result.unwrap_err().code.as_deref(),
                        Some("call_depth_exceeded")
                    );
                    assert_eq!(machine.metrics.entered_calls - entered, 512);
                    assert_eq!(machine.metrics.peak_calls, 512);
                } else {
                    let value = result.unwrap();
                    assert_eq!(s9_owner(&value, PAYLOAD).unwrap().error.is_some(), error);
                    assert_eq!(
                        machine.metrics.entered_calls - entered,
                        (n + 1 + extra) as usize
                    );
                }
                assert_eq!(
                    machine.metrics.retired_calls - retired,
                    machine.metrics.entered_calls - entered
                );
                assert_eq!(machine.metrics.retired_calls, machine.metrics.entered_calls);
                assert_eq!(machine.metrics.peak_string_payload_bytes, buffer.capacity);
                assert_eq!(host.call_depth, 0);
                s9_drained(&machine);
            }
        }
        });
        }
    });
}

#[test]
fn selected_string_local_scopes_and_saved_q_primaries_keep_actual_clone_fees_until_teardown() {
    for error in [false, true] {
        let leaf = if error { "err(text)" } else { "ok(text)" };
        let scoped = format!("fn Entry(text: str): str! {{ r: str! = {leaf}\nif (true) {{ inner: str! = r.clone()\ninner }}\nr\nr = r\nreturn r.clone() }}");
        let q2 = format!("fn Entry(text: str): str! {{ r: str! = {leaf}\ntry {{ return r.clone() }} finally {{ r\nr = r\nreturn r.clone() }}\nreturn r.clone() }}");
        let q3 = format!("fn Entry(text: str): str! {{ r: str! = {leaf}\ntry {{ return r.clone() }} finally {{ try {{ return r.clone() }} finally {{ r\nr = r\nreturn r.clone() }} }}\nreturn r.clone() }}");
        for (source, saved) in [(&scoped, 0), (&q2, 1), (&q3, 2)] {
            let mut machine = s9_machine(source);
            let mut host = production_interpreter(source);
            let (value, buffer) = string_input(PAYLOAD, 4096);
            let mut args = vec![value];
            let mut proof = S9Proof::new(buffer);
            machine
                .start_owned_once_with_hooks("Entry", &mut args, 0, false, &host)
                .unwrap();
            let mut saved_observed = false;
            let mut explicit_clones = 0;
            let mut final_value = None;
            for _ in 0..100_000 {
                if s9_explicit_clone_ticks(&mut machine, &mut host, buffer, &mut proof) {
                    explicit_clones += 1;
                } else if s9_at_clone(&machine) {
                    s9_clone_step(&mut machine, &mut host, buffer, &mut proof);
                } else {
                    match machine
                        .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut host)
                        .unwrap()
                    {
                        ChunkProgress::More { popped: 1 } => {
                            s9_observe(&machine, buffer, &mut proof);
                        }
                        ChunkProgress::Finished { result, .. } => {
                            let value = result
                                .unwrap()
                                .into_call_value(Span::default(), "function")
                                .unwrap();
                            let actual = s9_owner(&value, PAYLOAD).unwrap();
                            assert!(
                                proof.live.contains(&actual),
                                "Q winner was physically observed before teardown"
                            );
                            assert_eq!(proof.final_clone, Some(actual));
                            final_value = Some(value);
                            break;
                        }
                        _ => panic!("a synchronous single-step Q fixture cannot wait"),
                    }
                }
                let actual_saved = machine.finally.frames.as_slice().iter()
                    .filter(|frame| matches!(&frame.primary, Some(Ok(Flow::Return { value, .. } | Flow::Fail { value, .. })) if matches!(value, Value::Result { .. })))
                    .count();
                if actual_saved >= saved {
                    saved_observed = true;
                    let actual = s9_observe(&machine, buffer, &mut proof);
                    s9_budget(&mut machine, &actual, buffer);
                }
            }
            assert!(saved_observed);
            assert!(explicit_clones > saved);
            assert!(final_value.is_some(), "the bounded fixture completes");
            assert_eq!(machine.metrics.entered_finally_frames, saved);
            assert_eq!(machine.metrics.retired_finally_frames, saved);
            assert_eq!(host.call_depth, 0);
            s9_drained(&machine);
        }
    }
}

#[test]
fn selected_string_local_assignment_tombstones_original_and_never_spends_unused_input_pool() {
    const SOURCE: &str = "fn Entry(text: str): str! { r: str! = ok(text)\nr = r\nr\nreturn r }";
    for (text, capacity) in [(PAYLOAD, 4096), ("", 0), ("", 4096)] {
        let mut machine = s9_machine(SOURCE);
        let mut host = production_interpreter(SOURCE);
        let (value, buffer) = string_input(text, capacity);
        let mut args = vec![value];
        let mut proof = S9Proof::new(buffer);
        machine
            .start_owned_once_with_hooks("Entry", &mut args, 0, false, &host)
            .unwrap();
        pause_at(&mut machine, &mut host, s9_at_clone);
        let before = s9_observe(&machine, buffer, &mut proof);
        s9_clone_step(&mut machine, &mut host, buffer, &mut proof);
        let candidate = proof.final_clone.unwrap();
        if buffer.capacity != 0 {
            assert!(before.original_seen);
            let after = s9_observe(&machine, buffer, &mut proof);
            assert!(after.original_seen);
            assert_eq!(after.clone_bytes, candidate.string.unwrap().capacity);
        }
        // Execute the real Assign continuation that destroys the original r.
        let Some(Work::Bind(name, _, false, _)) = machine.work.as_slice().last() else {
            panic!("the actual assignment continuation follows its RHS clone");
        };
        assert_eq!(machine.code.symbol(*name), "r");
        assert!(matches!(
            machine
                .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut host)
                .unwrap(),
            ChunkProgress::More { popped: 1 }
        ));
        let assigned = s9_observe(&machine, buffer, &mut proof);
        assert!(!assigned.original_seen);
        assert!(!proof.original_live);
        assert_eq!(assigned.env, vec![candidate]);
        assert_eq!(assigned.clone_bytes, candidate.string.unwrap().capacity);
        assert_eq!(machine.string_payload_bytes, buffer.capacity);
        s9_budget(&mut machine, &assigned, buffer);
        let result = s9_finish(&mut machine, &mut host, 1, buffer, &mut proof).unwrap();
        let outgoing = s9_owner(&result, text).unwrap().string.unwrap();
        assert_eq!(outgoing.len, text.len());
        assert!(machine.metrics.peak_cloned_string_payload_bytes >= outgoing.capacity);
        assert_eq!(host.call_depth, 0);
    }
}

#[test]
fn selected_string_locals_leave_charged_async_and_other_whole_tree_boundaries_unmoved() {
    for source in [
        "async fn Entry(text: str): str! { r: str! = ok(text)\nreturn r }",
        "async fn Child(text: str): str! { return ok(text) }\nasync fn Entry(text: str): str! { r: str! = await Child(text)\nreturn r }",
        "fn Pass(value: str!): str! { return value }\nfn Entry(text: str): str! { r: str! = ok(text)\nreturn Pass(r) }",
        "fn Entry(text: str): str! { r: str! = ok(text.clone())\nreturn r }",
        "fn Entry(text: str): str! { r: str! = ok(text)\nreturn r.clone().clone() }",
        "fn Entry(text: str): str! { r: str! = ok(text)\nreturn ok(r?) }",
        "fn Entry(text: str): str! { r: str! = ok(text)\ntry { marker = 1 } catch (e) { marker = 2 }\nreturn r }",
    ] {
        let program = Parser::new(Lexer::new(&format!("{source}\nfn main() {{}}")).tokenize().unwrap())
            .parse_program().unwrap();
        crate::checker::Checker::new().check(&program).unwrap();
        let source_functions = functions(source);
        let entry = &source_functions["Entry"];
        assert!(!code::eligible_owned_named(&source_functions, entry, Limits::default()), "{source}");
        assert!(matches!(CodeHandle::compile_owned_named(&source_functions, entry, Limits::default()),
            Err(AdmissionError::Unsupported(_))), "{source}");
        if entry.is_async {
            let claimed = CodeHandle::compile_claimed_async_owned_result_string_entry(
                &source_functions, entry, Limits::default());
            assert!(claimed.unwrap().has_actual_string_result_locals(),
                "S10 no-await and S11 separately charged await locals are deliberate positives");
        }
        let mut host = production_interpreter(source);
        let (value, buffer) = string_input(PAYLOAD, 4096);
        let mut args = vec![value];
        let vector = (args.as_ptr(), args.len(), args.capacity());
        assert!(try_execute_owned(&mut host, entry, &mut args, 0).is_none());
        assert_eq!((args.as_ptr(), args.len(), args.capacity()), vector);
        assert_eq!((host.steps, host.call_depth), (0, 0));
        assert!(matches!(&args[0], Value::String(actual) if actual.as_ptr() as usize == buffer.pointer
            && actual.capacity() == buffer.capacity && actual == PAYLOAD));
    }
}
// Keep FinallyProbe unchanged and therefore unauthorized for S9. This wrapper
// permits only the real Interpreter's move/drop arbitration. Its fault injection
// changes control flow/TLS only; it does not construct or replace a String Value.
struct S9StringFaultProbe<'a> {
    inner: FinallyProbe<'a>,
    tighten_at: Option<(Span, RuntimeTermination)>,
    observed_cleanup: Option<RuntimeTermination>,
    handoff: Option<S11HandoffState>,
}

impl<'a> S9StringFaultProbe<'a> {
    fn new(
        host: &'a mut Interpreter,
        triggers: Vec<(Span, usize, FinallyProbeAction)>,
        tighten_at: Option<(Span, RuntimeTermination)>,
    ) -> Self {
        Self {
            inner: FinallyProbe::new(host, triggers),
            tighten_at,
            observed_cleanup: None,
            handoff: None,
        }
    }
}

impl ExecutionHooks for S9StringFaultProbe<'_> {
    fn observe_await_owned_handoff(
        &mut self,
        machine: &mut Machine,
        result: &KuResult<Value>,
        phase: AwaitOwnedHandoffPhase,
    ) {
        let Some(state) = self.handoff.as_mut() else {
            return;
        };
        let owner = result
            .as_ref()
            .ok()
            .and_then(s11_copy_owner)
            .expect("actual operation-local selected child result");
        let shape = s11_box_error_charge(owner);
        let string_bytes = owner.string.map_or(0, |string| string.capacity);
        assert!(
            machine.awaiting.is_none(),
            "actual ticket ownership has transferred into finish/emission locals"
        );
        assert_eq!(machine.await_payload_escrow, shape);
        assert_eq!(machine.await_string_payload_escrow.bytes, string_bytes);
        let mut census = s9_observe(machine, state.input, &mut state.proof);
        assert!(
            !census
                .owners
                .iter()
                .any(|physical| physical.boxed == owner.boxed),
            "operation-local owner occurs in no stable Env/operand/staged/pending/exit/Q location"
        );
        assert!(
            !state.proof.original_live,
            "child-returned backing receives no revived origin credit"
        );
        census.owners.push(owner); // Copy metadata, never another Value owner.
        census.clone_bytes += string_bytes;
        let (count, bytes) = s10_budget_totals(machine, &census, state.input);
        s11_check_await_quota(machine, count, bytes);
        let observer = matches!(phase, AwaitOwnedHandoffPhase::Observer);
        state.receipts.push(S11HandoffReceipt {
            observer,
            owner,
            shape,
            string_bytes,
            count,
            bytes,
        });
        if observer && state.fault == S11HandoffFault::NativeObserver {
            let first = RuntimeTermination::new(TerminationReason::TimedOut);
            let shorter = RuntimeTermination {
                reason: TerminationReason::Cancelled,
                cleanup_deadline: first.cleanup_deadline - Duration::from_millis(1),
            };
            // Existing Interpreter/Task arbitration latches the actual first
            // cause, and the real cleanup window retains its shortened D.
            let first_error: KuResult<Value> = Err(KuError::termination(first, Span::default()));
            self.inner.host.observe_termination(&first_error);
            let later_error: KuResult<Value> = Err(KuError::termination(shorter, Span::default()));
            self.inner.host.observe_termination(&later_error);
            let _cleanup = CleanupGuard::enter(first);
            drop(CleanupGuard::enter(shorter));
            let actual = current_cleanup_context().unwrap();
            assert_eq!(actual.reason, first.reason);
            assert_eq!(actual.cleanup_deadline, shorter.cleanup_deadline);
            state.native_context = Some(actual);
            panic!("S11 actual observer native panic while both escrow fees remain live");
        }
        if !observer
            && matches!(
                state.fault,
                S11HandoffFault::EmitCount | S11HandoffFault::EmitBytes
            )
        {
            state.saved_limits = Some(machine.value_limits);
            if state.fault == S11HandoffFault::EmitCount {
                let spare = machine.values.capacity() - machine.values.len();
                assert!(spare >= 1, "begin await reserved the actual emit slot");
                // FrameStack's frame limit checks required logical elements;
                // the full retained-capacity one-short checks above remain.
                // Leave exactly the current logical length, so the following
                // actual emit rejects its single additional element.
                machine.value_limits.max_frames = count - spare;
            } else {
                machine.value_limits.max_storage_bytes = bytes - 1;
            }
            // The following production emit consumes/drops the same actual
            // outcome and reports the original count/storage rejection.
        }
    }

    fn supports_selected_string_origin_moves(&self) -> bool {
        true
    }
    fn host(&mut self) -> &mut Interpreter {
        self.inner.host
    }

    fn prepare_scope_exit(&mut self, env: &mut Env, span: Span) -> KuResult<()> {
        ExecutionHooks::prepare_scope_exit(self.inner.host, env, span)
    }
    fn prepare_call_exit(&mut self, env: &mut Env, span: Span, kind: CallKind) -> KuResult<()> {
        ExecutionHooks::prepare_call_exit(self.inner.host, env, span, kind)
    }
    fn prepare_arguments_exit(
        &mut self,
        env: &mut Env,
        leases: &[BorrowLease],
        values: &mut [Value],
        span: Span,
    ) -> KuResult<()> {
        ExecutionHooks::prepare_arguments_exit(self.inner.host, env, leases, values, span)
    }

    fn tick(&mut self, span: Span) -> KuResult<()> {
        if self
            .tighten_at
            .as_ref()
            .is_some_and(|(target, _)| *target == span)
        {
            let (_, context) = self.tighten_at.take().unwrap();
            drop(CleanupGuard::enter(context));
            self.observed_cleanup = current_cleanup_context();
        }
        ExecutionHooks::tick(&mut self.inner, span)
    }

    fn finish_scope(
        &mut self,
        env: &mut Env,
        result: KuResult<Flow>,
        span: Span,
        pop: bool,
    ) -> KuResult<Flow> {
        ExecutionHooks::finish_scope(self.inner.host, env, result, span, pop)
    }
    fn finish_scope_with_feedback(
        &mut self,
        env: &mut Env,
        result: KuResult<Flow>,
        span: Span,
        pop: bool,
        feedback: &Cell<Option<RuntimeTermination>>,
    ) -> KuResult<Flow> {
        ExecutionHooks::finish_scope_with_feedback(
            self.inner.host,
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
        ExecutionHooks::finish_call(self.inner.host, env, leases, result, span, kind)
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
        ExecutionHooks::finish_call_with_feedback(
            self.inner.host,
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
        ExecutionHooks::finish_call_value(self.inner.host, result, span, kind)
    }
    fn finish_arguments(
        &mut self,
        env: &mut Env,
        leases: Vec<BorrowLease>,
        values: Vec<Value>,
        result: KuResult<Flow>,
        span: Span,
    ) -> KuResult<Flow> {
        ExecutionHooks::finish_arguments(self.inner.host, env, leases, values, result, span)
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
        ExecutionHooks::finish_arguments_with_feedback(
            self.inner.host,
            env,
            leases,
            values,
            result,
            span,
            feedback,
        )
    }
    fn finish_arguments_value(&mut self, result: KuResult<Flow>, span: Span) -> KuResult<Flow> {
        ExecutionHooks::finish_arguments_value(self.inner.host, result, span)
    }
    fn prepare_finally(
        &mut self,
        env: &Env,
        primary: &KuResult<Flow>,
        span: Span,
    ) -> cancellation::FinallyMode {
        ExecutionHooks::prepare_finally(&mut self.inner, env, primary, span)
    }
    fn finish_finally(
        &mut self,
        primary: KuResult<Flow>,
        replacement: KuResult<Flow>,
        mode: cancellation::FinallyMode,
        span: Span,
    ) -> KuResult<Flow> {
        ExecutionHooks::finish_finally(&mut self.inner, primary, replacement, mode, span)
    }
}

// This has one owned String parameter, two real typed local Result cells, and
// no await/Task/borrow/Result parameter. The only String allocation after entry
// is the admitted clone Work.
const S9_FAULT_SOURCE: &str =
    "fn Entry(text: str): str! { r: str! = ok(text)\ns: str! = r\nreturn s }";

fn s9_input_identity(
    args: &[Value],
    capacity: usize,
) -> (usize, usize, usize, usize, usize, usize) {
    let Value::String(text) = &args[0] else {
        panic!("one real String input")
    };
    assert_eq!(text.as_str(), PAYLOAD);
    (
        args.as_ptr() as usize,
        capacity,
        args.len(),
        text.as_ptr() as usize,
        text.capacity(),
        text.len(),
    )
}

// Snapshot only metadata. In particular, neither the source Value nor a Work
// carrying Values is cloned by the fixture.
fn s9_pause_identity(machine: &Machine) -> String {
    format!(
        "{:?}|{}:{}|{}:{}|{}:{}|{}:{}|{}:{}:{}:{}:{}|{:?}|{:?}",
        machine.state,
        machine.work.as_slice().as_ptr() as usize,
        machine.work.len(),
        machine.calls.as_slice().as_ptr() as usize,
        machine.calls.len(),
        machine.values.as_slice().as_ptr() as usize,
        machine.values.len(),
        machine.exits.as_slice().as_ptr() as usize,
        machine.exits.len(),
        machine.argument_bytes,
        machine.string_payload_bytes,
        machine.cloned_string_payload_bytes,
        machine.result_wrapper_bytes,
        machine.error_leaf_bytes,
        machine
            .original_string
            .map(|value| (value.pointer, value.capacity, value.still_live)),
        machine.metrics
    )
}

#[test]
fn selected_string_local_default_hooks_and_old_starts_preserve_inputs_then_resume() {
    let mut kernel = s9_machine(S9_FAULT_SOURCE);
    let mut host = production_interpreter(S9_FAULT_SOURCE);
    let (text, buffer) = string_input(PAYLOAD, 4096);
    let mut args = vec![text];
    let input_before = s9_input_identity(&args, args.capacity());
    let ready_before = s9_pause_identity(&kernel);
    assert!(!kernel.string_origin_authorized);

    let old_error = kernel
        .start_owned_once("Entry", &mut args, 0, false)
        .unwrap_err();
    assert!(old_error.message.contains("does not support this source"));
    assert_eq!(s9_input_identity(&args, args.capacity()), input_before);
    assert_eq!(s9_pause_identity(&kernel), ready_before);
    {
        let probe = FinallyProbe::new(&mut host, vec![]);
        let error = kernel
            .start_owned_once_with_hooks("Entry", &mut args, 0, false, &probe)
            .unwrap_err();
        assert!(error.message.contains("does not support this source"));
        assert!(probe.ticks.is_empty());
    }
    assert_eq!(s9_input_identity(&args, args.capacity()), input_before);
    assert_eq!(s9_pause_identity(&kernel), ready_before);
    assert_eq!((host.steps, host.call_depth), (0, 0));
    assert!(!kernel.string_origin_authorized);

    // The scalar/function-value starts must not bypass ownership authorization.
    let error = kernel.start_once("Entry", &args, 0, false).unwrap_err();
    assert!(error.message.contains("does not support this source"));
    assert_eq!(s9_input_identity(&args, args.capacity()), input_before);
    assert_eq!(s9_pause_identity(&kernel), ready_before);

    let function = kernel.code.find_function("Entry").unwrap();
    let span = kernel.code.function(function).span;
    let error = kernel
        .start_function_once(function, &args, 0, false, span)
        .unwrap_err();
    assert!(error.message.contains("does not support this source"));
    assert_eq!(s9_input_identity(&args, args.capacity()), input_before);
    assert_eq!(s9_pause_identity(&kernel), ready_before);

    kernel
        .start_owned_once_with_hooks("Entry", &mut args, 0, false, &host)
        .unwrap();
    assert!(args.is_empty() && args.capacity() == 0);
    pause_at(&mut kernel, &mut host, s9_at_clone);
    let mut proof = S9Proof::new(buffer);
    let before = s9_observe(&kernel, buffer, &mut proof);
    assert_eq!(before.env.len(), 1);
    let paused = s9_pause_identity(&kernel);
    let steps = host.steps;
    {
        let mut probe = FinallyProbe::new(&mut host, vec![]);
        let error = kernel
            .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut probe)
            .err()
            .unwrap();
        assert!(error.message.contains("does not support this source"));
        assert!(probe.ticks.is_empty());
    }
    assert_eq!(host.steps, steps);
    assert_eq!(s9_pause_identity(&kernel), paused);
    let after = s9_observe(&kernel, buffer, &mut proof);
    assert_eq!(after.env, before.env);
    assert_eq!(after.owners, before.owners);
    assert_eq!(after.clone_bytes, before.clone_bytes);
    let value = s9_finish(&mut kernel, &mut host, 1, buffer, &mut proof).unwrap();
    assert!(s9_owner(&value, buffer.text).unwrap().string.is_some());
    s9_drained(&kernel);

    // Original terminal-only String graphs still admit their old API and the
    // default-false probe. Do not turn generic string contract into authorization.
    let mut legacy_machine = machine(RELAY, "Relay", Limits::default());
    let mut legacy_host = production_interpreter(RELAY);
    assert!(!legacy_machine.code.has_actual_string_result_locals());
    let (mut args, pointer, capacity) = input(0);
    legacy_machine
        .start_owned_once("Relay", &mut args, 0, false)
        .unwrap();
    {
        let mut probe = FinallyProbe::new(&mut legacy_host, vec![]);
        assert!(matches!(
            legacy_machine
                .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut probe)
                .unwrap(),
            ChunkProgress::More { popped: 1 }
        ));
    }
    check_result(
        &finish(&mut legacy_machine, &mut legacy_host, 1, pointer, capacity).unwrap(),
        pointer,
        capacity,
    );
    assert_owned_drained(&legacy_machine);
}

fn s9_native_abandonment_proof() {
    let source = functions(S9_FAULT_SOURCE);
    let target = stmt_span(&source["Entry"].body[2]);
    let mut machine = s9_machine(S9_FAULT_SOURCE);
    let mut host = production_interpreter(S9_FAULT_SOURCE);
    let (text, buffer) = string_input(PAYLOAD, 4096);
    let mut args = vec![text];
    machine
        .start_owned_once_with_hooks("Entry", &mut args, 0, false, &host)
        .unwrap();
    let mut proof = S9Proof::new(buffer);
    pause_at(&mut machine, &mut host, s9_at_clone);
    s9_clone_step(&mut machine, &mut host, buffer, &mut proof);
    assert!(matches!(
        machine.work.as_slice().last(),
        Some(Work::Bind(..))
    ));
    assert!(matches!(
        machine
            .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut host)
            .unwrap(),
        ChunkProgress::More { popped: 1 }
    ));
    let before = s9_observe(&machine, buffer, &mut proof);
    assert_eq!(before.env.len(), 2);
    assert_eq!(before.owners.len(), 2);
    s9_assert_distinct(before.env[0], before.env[1]);

    let now = Instant::now();
    let original = RuntimeTermination {
        reason: TerminationReason::TimedOut,
        cleanup_deadline: now,
    };
    let shorter = RuntimeTermination {
        reason: TerminationReason::Cancelled,
        cleanup_deadline: now.checked_sub(Duration::from_millis(1)).unwrap(),
    };
    let before_cleanup = current_cleanup_context();
    let before_execution = crate::runtime::task::current_execution_termination();
    let mut probe = S9StringFaultProbe::new(
        &mut host,
        vec![(target, 1, FinallyProbeAction::Panic)],
        Some((target, shorter)),
    );
    let native = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _execution = ExecutionTerminationGuard::enter();
        let _cleanup = CleanupGuard::enter(original);
        for _ in 0..100_000 {
            assert!(matches!(
                machine
                    .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut probe)
                    .unwrap(),
                ChunkProgress::More { popped: 1 }
            ));
        }
        panic!("the bounded real return safepoint was not reached");
    }));
    assert_eq!(
        native.unwrap_err().downcast_ref::<&str>().copied(),
        Some("native finally probe panic")
    );
    assert!(probe.inner.triggers.is_empty());
    assert_eq!(
        probe.observed_cleanup,
        Some(RuntimeTermination {
            reason: original.reason,
            cleanup_deadline: shorter.cleanup_deadline,
        })
    );
    assert_eq!(machine.state, MachineState::Aborted);
    assert_eq!(machine.aborted_release, probe.observed_cleanup);
    drop(probe);
    // The dispatch catch settles operation locals. Both real Env owners are
    // still alive and separately charged before the actual Machine Drop.
    let after = s9_observe(&machine, buffer, &mut proof);
    assert_eq!(after.env, before.env);
    assert_eq!(after.owners, before.owners);
    assert_eq!(after.clone_bytes, before.clone_bytes);
    assert_eq!(current_cleanup_context(), before_cleanup);
    assert_eq!(
        crate::runtime::task::current_execution_termination(),
        before_execution
    );
    drop(machine);
    assert_eq!(current_cleanup_context(), before_cleanup);
    assert_eq!(
        crate::runtime::task::current_execution_termination(),
        before_execution
    );
    assert_eq!(host.call_depth, 0);
}

// Physical Drop limitation: String and Box have no user destructor. Do not add
// a synthetic Task or fabricated Drop callback into this strict taskless graph.
// This draft validates real retained owner fees before Drop and TLS after Drop;
// it does not claim allocator-level post-free observation.
//
// Existing old terminal-only/old scalar poison tests remain unchanged.

#[test]
fn selected_string_local_poison_uses_only_new_taskless_classifier_and_drains_real_source() {
    let mut machine = s9_machine(S9_FAULT_SOURCE);
    let mut host = production_interpreter(S9_FAULT_SOURCE);
    let (text, buffer) = string_input(PAYLOAD, 4096);
    let mut args = vec![text];
    machine
        .start_owned_once_with_hooks("Entry", &mut args, 0, false, &host)
        .unwrap();
    pause_at(&mut machine, &mut host, s9_at_clone);
    let mut proof = S9Proof::new(buffer);
    let before = s9_observe(&machine, buffer, &mut proof);
    assert_eq!(before.env.len(), 1);
    let original_charge = machine.frame().env.selected_string_result_charge();

    // The fault takes the actual source binding lock, obtains a genuine clone
    // from the private clone operation, then abandons that owner on unwind.
    // This is an operation-layer native fault, not a fabricated Machine Work.
    let mut candidate = None;
    let mut returned_charge = None;
    let now = Instant::now();
    let first = RuntimeTermination {
        reason: TerminationReason::TimedOut,
        cleanup_deadline: now,
    };
    let shorter = RuntimeTermination {
        reason: TerminationReason::Cancelled,
        cleanup_deadline: now.checked_sub(Duration::from_millis(1)).unwrap(),
    };
    let mut observed_cleanup = None;
    let native = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _cleanup = CleanupGuard::enter(first);
        machine
            .frame()
            .env
            .with_value::<()>("r", Span::default(), |source| {
                let mut observed = StringCloneCharge::default();
                let (value, charge) = clone_selected_string_result(
                    source,
                    unlimited_local_clone_policy(),
                    &mut observed,
                )?;
                let source_metadata = s9_owner(source, buffer.text).unwrap();
                let candidate_metadata = s9_owner(&value, buffer.text).unwrap();
                assert_eq!(source_metadata, before.env[0]);
                s9_assert_distinct(source_metadata, candidate_metadata);
                // Both actual source and clone are alive at the comparison above.
                candidate = Some(candidate_metadata);
                returned_charge = Some(charge);
                drop(CleanupGuard::enter(shorter));
                observed_cleanup = current_cleanup_context();
                let _operation_owner = value;
                panic!("actual selected String clone abandonment under source lock");
            })
            .unwrap();
    }));
    assert_eq!(
        native.unwrap_err().downcast_ref::<&str>().copied(),
        Some("actual selected String clone abandonment under source lock")
    );
    let candidate = candidate.unwrap();
    let fee = returned_charge.unwrap();
    assert_eq!(fee.string_bytes, candidate.string.unwrap().capacity);
    assert_eq!(fee.payload.box_count, 1);
    assert_eq!(fee.payload.box_bytes, std::mem::size_of::<Value>());
    assert_eq!(
        observed_cleanup,
        Some(RuntimeTermination {
            reason: first.reason,
            cleanup_deadline: shorter.cleanup_deadline,
        })
    );
    assert!(current_cleanup_context().is_none());

    // The original public/scalar poison contract still rejects this success
    // String. S9 uses only its separately gated strict taskless APIs.
    assert_poison_simple_contract(&mut machine.frame().env, "r", false);
    let span = Span::default();
    assert_eq!(
        machine
            .frame()
            .env
            .current_scope_selected_string_task_count(span)
            .unwrap(),
        0
    );
    let mut proxies = Vec::new();
    machine
        .frame()
        .env
        .collect_current_scope_selected_string_tasks_reserved(&mut proxies, span)
        .unwrap();
    assert_eq!((proxies.len(), proxies.capacity()), (0, 0));
    assert_eq!(
        machine.frame().env.selected_string_result_charge(),
        original_charge
    );
    machine.synchronize_result_wrapper_charge();
    let after = s9_observe(&machine, buffer, &mut proof);
    assert_eq!(after.env, before.env);
    assert_eq!(after.owners, before.owners);
    assert_eq!(
        after.clone_bytes, 0,
        "the operation-local clone physically unwound"
    );
    // No Machine peak claim is inferred from the abandoned external operation.
    // Drive the poisoned clone Work directly: the independent normal-clone
    // observer intentionally uses ordinary with_value and must not unwrap a
    // poisoned source before the runtime can report its own genuine error.
    assert!(s9_at_clone(&machine));
    assert!(matches!(
        machine
            .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut host)
            .unwrap(),
        ChunkProgress::More { popped: 1 }
    ));
    assert!(
        matches!(machine.pending.as_ref(), Some(Err(error)) if error.message.contains("poisoned"))
    );
    let error = s9_finish(&mut machine, &mut host, 1, buffer, &mut proof).unwrap_err();
    assert!(error.message.contains("poisoned"));
    s9_drained(&machine);
    assert!(current_cleanup_context().is_none());
    assert!(crate::runtime::task::current_execution_termination().is_none());
    assert_eq!(host.call_depth, 0);
}

#[test]
fn selected_string_local_real_native_panic_drop_preserves_first_cleanup_cause_and_tls() {
    s9_native_abandonment_proof();
}

#[test]
fn selected_string_local_protected_cancel_drains_real_finally_clones_without_replacing_first_cause()
{
    const SOURCE: &str = "fn Entry(text: str): str! { r: str! = ok(text)\ntry { marker = 1\nmarker = 2 } finally { r\nr = r }\nreturn r }";
    let source = functions(SOURCE);
    let Stmt::Try { body, .. } = &source["Entry"].body[1] else {
        panic!("the real protected fixture")
    };
    let target = stmt_span(&body[1]);
    let first = RuntimeTermination {
        reason: TerminationReason::Cancelled,
        cleanup_deadline: Instant::now() + HTTP_HANDLER_CLEANUP_GRACE,
    };
    let mut kernel = s9_machine(SOURCE);
    let mut host = production_interpreter(SOURCE);
    let (value, buffer) = string_input(PAYLOAD, 4096);
    let mut args = vec![value];
    kernel
        .start_owned_once_with_hooks("Entry", &mut args, 0, false, &host)
        .unwrap();
    let mut proof = S9Proof::new(buffer);
    let before_cleanup = current_cleanup_context();
    let before_execution = crate::runtime::task::current_execution_termination();
    let execution = ExecutionTerminationGuard::enter();
    let mut probe = S9StringFaultProbe::new(
        &mut host,
        vec![(target, 1, FinallyProbeAction::Terminate(first))],
        None,
    );
    let mut finished = None;
    for _ in 0..100_000 {
        match kernel
            .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut probe)
            .unwrap()
        {
            ChunkProgress::More { popped: 1 } => {
                s9_observe(&kernel, buffer, &mut proof);
            }
            ChunkProgress::Finished { result, .. } => {
                finished = Some(match result {
                    Err(error) => error,
                    Ok(_) => panic!("the protected cancellation must preserve its real error"),
                });
                break;
            }
            _ => panic!("the bounded synchronous protected fixture cannot wait"),
        }
    }
    let error = finished.expect("protected cleanup completes under its original deadline");
    assert_eq!(error.runtime_termination(), Some(first));
    assert_eq!(probe.inner.protected, vec![first]);
    assert!(probe.inner.triggers.is_empty());
    assert!(
        proof.peak_clone_bytes != 0,
        "the protected finally really cloned the selected success"
    );
    assert_eq!(kernel.metrics.entered_finally_frames, 1);
    assert_eq!(kernel.metrics.retired_finally_frames, 1);
    s9_drained(&kernel);
    drop(probe);
    drop(execution);
    assert_eq!(host.call_depth, 0);
    assert_eq!(current_cleanup_context(), before_cleanup);
    assert_eq!(
        crate::runtime::task::current_execution_termination(),
        before_execution
    );
}

#[test]
fn selected_string_local_native_drop_keeps_the_same_worker_next_task_healthy() {
    let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
    let (tx, rx) = mpsc::channel();
    let first = runtime
        .spawn(move || {
            s9_native_abandonment_proof();
            tx.send((
                std::thread::current().id(),
                crate::runtime::task::current_task_id(),
                current_cleanup_context().is_none(),
                crate::runtime::task::current_execution_termination().is_none(),
            ))
            .unwrap();
            Ok(Value::Null)
        })
        .unwrap();
    let abandoned = rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(first.await_result().unwrap(), Value::Null);
    retire_s7_task(first);
    let (tx, rx) = mpsc::channel();
    let healthy = runtime
        .spawn(move || {
            let mut host = production_interpreter(S9_FAULT_SOURCE);
            let (value, buffer) = string_input(PAYLOAD, 4096);
            let result = host.call_function("Entry", vec![value], Span::default(), 0)?;
            let actual = s9_owner(&result, buffer.text).unwrap();
            assert!(actual.string.is_some());
            tx.send((
                std::thread::current().id(),
                crate::runtime::task::current_task_id(),
                host.call_depth,
                current_cleanup_context().is_none(),
                crate::runtime::task::current_execution_termination().is_none(),
            ))
            .unwrap();
            Ok(result)
        })
        .unwrap();
    let next = rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let returned = healthy.await_result().unwrap();
    // await_result follows its existing clone contract. Check its content,
    // never infer that its Box/buffer is the earlier worker-owned allocation.
    assert!(s9_owner(&returned, PAYLOAD).unwrap().string.is_some());
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
    assert_eq!(abandoned.0, next.0);
    assert_ne!(abandoned.1, 0);
    assert_ne!(abandoned.1, next.1);
    assert!(abandoned.2 && abandoned.3 && next.3 && next.4);
    assert_eq!(next.2, 0);
}

// The claimant stays on its actual Task worker. S9's private census is reused
// unchanged: it reads every real Env, operand, argument exit, Q primary and
// transferring outcome. This wrapper never constructs a substitute Value.
const S10_WAIT: Duration = Duration::from_secs(3);

fn s10_wait_result(task: &crate::runtime::task::TaskHandle) -> KuResult<Value> {
    let deadline = Instant::now() + S10_WAIT;
    loop {
        // Only Copy readiness crosses the existing result lock. This fixture
        // holds the sole consumer/owner, so the published slot cannot be taken
        // between this observation and the original ordinary consuming await.
        // Do not use Diagnostic await_timeout: it would clone without consuming.
        if task.test_with_result_slot(|slot| slot.is_some())? {
            return task.await_result();
        }
        assert!(
            Instant::now() < deadline,
            "bounded actual result publication"
        );
        std::thread::yield_now();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum S10Pause {
    None,
    Running,
    Produced,
    Protected(Span),
    AwaitWaiting,
    AwaitReady,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum S10Fault {
    None,
    CloneCount,
    CloneBytes,
    NativeClone,
    NativeReady,
}

#[derive(Clone, Copy)]
struct S10Plan {
    chunk: usize,
    deep: bool,
    from_code: bool,
    selection_only: bool,
    await_locals: bool,
    payload_text: Option<&'static str>,
    task_errors: bool,
    pause: S10Pause,
    fault: S10Fault,
}

impl S10Plan {
    fn normal(chunk: usize) -> Self {
        Self {
            chunk,
            deep: false,
            from_code: false,
            selection_only: false,
            await_locals: false,
            payload_text: None,
            task_errors: false,
            pause: S10Pause::None,
            fault: S10Fault::None,
        }
    }
}

#[derive(Debug)]
struct S10Finished {
    thread: std::thread::ThreadId,
    task: i64,
    calls: usize,
    retired: usize,
    peak_calls: usize,
    finally: usize,
    retired_finally: usize,
    peak_boxes: usize,
    peak_clones: usize,
    peak_errors: usize,
    observed_peak_boxes: usize,
    observed_peak_clones: usize,
    distinct_clones: usize,
    checked_budgets: usize,
    max_saved: usize,
    original_retired: bool,
    depth_512: bool,
    outgoing: Option<S9Owner>,
    rejected_source: bool,
}

#[derive(Debug)]
struct S10Native {
    task: i64,
    thread: std::thread::ThreadId,
    source: S9Owner,
    clone: S9Owner,
    first: RuntimeTermination,
    observed: RuntimeTermination,
}

#[derive(Debug)]
enum S10Witness {
    Entered {
        task: i64,
        thread: std::thread::ThreadId,
        selected: bool,
        legacy: bool,
        steps: usize,
        calls: usize,
    },
    Progress {
        dispatches: usize,
        depth: usize,
    },
    Paused {
        stage: S10Pause,
        owner: Option<Box<S9Owner>>,
    },
    Native(Box<S10Native>),
    Finished(Box<S10Finished>),
    AwaitWaiting,
    AwaitChild(Box<S11ChildWitness>),
    AwaitReady(Box<S11Ready>),
    AwaitBound(S9Owner),
}

struct S10Runner {
    inner: ClaimedNamedTask,
    plan: S10Plan,
    input: Buffer,
    proof: S9Proof,
    tx: mpsc::Sender<S10Witness>,
    release: Option<mpsc::Receiver<()>>,
    thread: std::thread::ThreadId,
    dispatches: usize,
    distinct_clones: usize,
    checked_budgets: usize,
    max_saved: usize,
    depth_512: bool,
    faulted: bool,
    paused: bool,
    rejected_source: bool,
    await_probe: S11Probe,
}

fn s10_budget_totals(machine: &mut Machine, actual: &S9Census, input: Buffer) -> (usize, usize) {
    s9_budget(machine, actual, input);
    let count = machine.values.capacity()
        + machine.argument_capacity
        + machine.exit_handle_capacity
        + machine.retirement_capacity
        + machine.lease_capacity
        + machine.lease_root_bytes / std::mem::size_of::<Value>()
        + actual.owners.len()
        + actual
            .owners
            .iter()
            .filter_map(|owner| owner.error)
            .map(|error| error.count)
            .sum::<usize>();
    let bytes = machine.values.charged_storage_bytes()
        + machine.argument_bytes
        + machine.exit_handle_bytes
        + machine.retirement_bytes
        + machine.lease_bytes
        + machine.lease_root_bytes
        + input.capacity
        + actual.clone_bytes
        + actual.owners.len() * std::mem::size_of::<Value>()
        + actual
            .owners
            .iter()
            .filter_map(|owner| owner.error)
            .map(|error| error.bytes)
            .sum::<usize>();
    (count, bytes)
}

impl S10Runner {
    fn pause(&mut self, stage: S10Pause, owner: Option<S9Owner>) {
        self.paused = true;
        self.tx
            .send(S10Witness::Paused {
                stage,
                owner: owner.map(Box::new),
            })
            .unwrap();
        self.release
            .take()
            .expect("one real claimant pause")
            .recv_timeout(S10_WAIT)
            .expect("finite checkpoint release");
    }
}

impl crate::runtime::task::LocalTaskRunner for S10Runner {
    fn dispatch(&mut self, _driver_budget: NonZeroUsize) -> std::ops::ControlFlow<KuResult<Value>> {
        use std::ops::ControlFlow;
        self.dispatches += 1;
        assert!(self.dispatches <= 100_000, "the source has finite work");
        assert_eq!(std::thread::current().id(), self.thread);
        assert_eq!(crate::runtime::task::current_task_id(), self.inner.task_id);
        if self.plan.selection_only {
            return ControlFlow::Break(Ok(Value::Null));
        }
        let mut chunk = self.plan.chunk;
        let mut clone_source = None;
        let mut saved_limits = None;
        let machine = self
            .inner
            .machine
            .as_mut()
            .expect("real selected S10 claimant");
        assert!(self.inner.legacy.is_none());
        if !self.plan.await_locals {
            assert!(
                machine.awaiting.is_none(),
                "all original S10 fixtures remain no-await"
            );
        } else {
            if !self.await_probe.child_seen {
                // Observe the actual initial Task ownership transfer, rather
                // than comparing a task or payload that coarse chunks retired.
                chunk = 1;
            }
            if !self.await_probe.child_seen
                && machine.pending.is_none()
                && matches!(
                    machine.work.as_slice().last(),
                    Some(Work::AwaitLocalOwnedStringOperand(_))
                )
            {
                if let Some(Value::Task(child)) = machine.values.as_slice().last() {
                    self.await_probe.child_seen = true;
                    self.tx
                        .send(S10Witness::AwaitChild(Box::new(S11ChildWitness(
                            child.retirement_witness(),
                        ))))
                        .unwrap();
                }
            }
        }
        if self.plan.await_locals {
            if let Some(frame) = machine.awaiting.as_ref() {
                if machine.state == MachineState::Waiting
                    && frame.ticket.test_outcome().is_none()
                    && self.await_probe.yielded_span != Some(frame.span)
                {
                    // The existing LocalDriver polls the real ticket next. The
                    // fixture adds no Work tick and does not inject an outcome.
                    self.await_probe.yielded_span = Some(frame.span);
                    return ControlFlow::Continue(());
                }
                if let Some(Ok(value)) = frame.ticket.test_outcome() {
                    let owner =
                        s11_measured_owner(value, self.proof.payload_text, self.proof.task_errors)
                            .unwrap();
                    let before = s9_observe(machine, self.input, &mut self.proof);
                    assert!(before.owners.contains(&owner));
                    let (count, bytes) = s10_budget_totals(machine, &before, self.input);
                    s11_check_await_quota(machine, count, bytes);
                    self.checked_budgets += 1;
                    if !self.await_probe.ready_seen {
                        self.await_probe.ready_seen = true;
                        if self.plan.chunk == 1 {
                            self.await_probe.first_bind = Some(owner);
                        }
                        self.tx
                            .send(S10Witness::AwaitReady(Box::new(S11Ready {
                                owner,
                                input_capacity: self.input.capacity,
                                current_clone_bytes: before.clone_bytes,
                                cached_string_bytes: machine.await_string_payload_snapshot.bytes,
                                stable_clone_bytes: machine.cloned_string_payload_bytes,
                                count,
                                bytes,
                                origin_live: self.proof.original_live,
                            })))
                            .unwrap();
                    }
                    if self.plan.pause == S10Pause::AwaitReady && !self.paused {
                        self.paused = true;
                        self.tx
                            .send(S10Witness::Paused {
                                stage: S10Pause::AwaitReady,
                                owner: Some(Box::new(owner)),
                            })
                            .unwrap();
                        self.release
                            .take()
                            .expect("one await pause")
                            .recv_timeout(S10_WAIT)
                            .unwrap();
                    }
                    if self.plan.fault == S10Fault::NativeReady && !self.faulted {
                        self.faulted = true;
                        let first = RuntimeTermination::new(TerminationReason::TimedOut);
                        let _cleanup = CleanupGuard::enter(first);
                        self.tx
                            .send(S10Witness::Paused {
                                stage: S10Pause::AwaitReady,
                                owner: Some(Box::new(owner)),
                            })
                            .unwrap();
                        // The real runner / Machine / ticket unwind on this worker;
                        // no payload copy or fake outgoing witness survives it.
                        panic!("S11 actual Ready await local native abandonment");
                    }
                }
            }
        }
        let depth = machine.calls.len();
        // Deep terminal relays cannot contain a String Result local above the
        // root. Their previous completed chunk already had a full census, and
        // this no-await Machine has no foreign writer between dispatches. Keep
        // every post-dispatch census; do not repeat it on the stable empty
        // middle prefix. Shallow/clone and exact-512 checkpoints stay fresh.
        let before =
            if !self.plan.deep || depth <= 32 || depth == MAX_CALL_DEPTH || self.dispatches == 1 {
                Some(s9_observe(machine, self.input, &mut self.proof))
            } else {
                assert!(
                    !s9_at_clone(machine),
                    "deep middle relays have no local clone"
                );
                assert_eq!(self.plan.pause, S10Pause::None);
                assert_eq!(self.plan.fault, S10Fault::None);
                assert!(machine.captures.is_none());
                None
            };
        if self.plan.deep {
            assert!(depth <= MAX_CALL_DEPTH);
            if depth == MAX_CALL_DEPTH && !self.depth_512 {
                let before = before.as_ref().expect("fresh exact-512 physical census");
                assert!(
                    before.env.len() <= 1,
                    "terminal relays install no artificial keepers"
                );
                s10_budget_totals(machine, before, self.input);
                self.checked_budgets += 1;
                self.depth_512 = true;
            }
            chunk = if depth == MAX_CALL_DEPTH {
                // Keep every actual Work at 512 observable until its leaf
                // completes or the first real 513th enter is rejected.
                1
            } else if !self.depth_512 {
                chunk.min((MAX_CALL_DEPTH - depth).max(1))
            } else if depth <= 32 {
                1
            } else {
                chunk
            };
        }
        if chunk == 1 && s9_at_clone(machine) {
            let before = before
                .as_ref()
                .expect("deep terminal middle frames contain no selected local clone");
            let Work::CloneSelectedStringResult(name, span) =
                machine.work.as_slice().last().unwrap()
            else {
                unreachable!()
            };
            clone_source = Some(
                machine
                    .calls
                    .as_slice()
                    .last()
                    .unwrap()
                    .env
                    .with_value(
                        machine.code.symbol(*name),
                        machine.code.span(*span),
                        |value| {
                            Ok(s11_measured_owner(
                                value,
                                self.proof.payload_text,
                                self.proof.task_errors,
                            )
                            .unwrap())
                        },
                    )
                    .unwrap(),
            );
            assert!(before.env.contains(&clone_source.unwrap()));
            if self.checked_budgets == 0 {
                machine.reserve_values(1, Span::default()).unwrap();
                let actual = s9_observe(machine, self.input, &mut self.proof);
                s10_budget_totals(machine, &actual, self.input);
                self.checked_budgets += 1;
            }
            if !self.faulted
                && matches!(self.plan.fault, S10Fault::CloneCount | S10Fault::CloneBytes)
            {
                machine.reserve_values(1, Span::default()).unwrap();
                let actual = s9_observe(machine, self.input, &mut self.proof);
                let (count, bytes) = s10_budget_totals(machine, &actual, self.input);
                self.checked_budgets += 1;
                let source = clone_source.unwrap();
                saved_limits = Some(machine.value_limits);
                if self.plan.fault == S10Fault::CloneCount {
                    // A new Result always needs a Box, before any String/Map
                    // allocation. The current count is independently folded.
                    machine.value_limits.max_frames = count;
                } else {
                    // A nonempty success buffer or real false Map cannot fit
                    // into a lone additional Box. This is pre-allocation
                    // minimum rejection; no clone capacity is fabricated.
                    assert!(!self.input.text.is_empty());
                    assert!(source.string.is_some() || source.error.is_some());
                    machine.value_limits.max_storage_bytes = bytes + std::mem::size_of::<Value>();
                }
                self.faulted = true;
            }
        }
        let protected_ready = match self.plan.pause {
            S10Pause::Protected(target) if !self.paused => {
                matches!(machine.work.as_slice().last(), Some(Work::Statement(stmt))
                    if machine.code.span(machine.code.statement(*stmt).span) == target)
            }
            _ => false,
        };
        if protected_ready {
            assert!(!machine.finally.frames.as_slice().is_empty());
            self.pause(self.plan.pause, None);
        }
        // Only the actual named task performs normal dispatch/finish conversion.
        // Interpreter step counts are not a substitute for popped Work receipts.
        let result = self.inner.dispatch(NonZeroUsize::new(chunk).unwrap());
        let machine = self.inner.machine.as_mut().unwrap();
        if let Some(saved) = saved_limits {
            machine.value_limits = saved;
        }
        if matches!(result, ControlFlow::Continue(())) {
            let actual = s9_observe(machine, self.input, &mut self.proof);
            if self.plan.await_locals {
                if machine.state == MachineState::Waiting
                    && machine
                        .awaiting
                        .as_ref()
                        .is_some_and(|frame| frame.ticket.test_outcome().is_none())
                    && !self.await_probe.waiting_seen
                {
                    self.await_probe.waiting_seen = true;
                    self.tx.send(S10Witness::AwaitWaiting).unwrap();
                    if self.plan.pause == S10Pause::AwaitWaiting && !self.paused {
                        self.paused = true;
                        self.tx
                            .send(S10Witness::Paused {
                                stage: S10Pause::AwaitWaiting,
                                owner: None,
                            })
                            .unwrap();
                        self.release
                            .take()
                            .expect("one true Waiting pause")
                            .recv_timeout(S10_WAIT)
                            .unwrap();
                    }
                }
                if let Some(ticket) = self.await_probe.first_bind {
                    if actual.env.contains(&ticket) {
                        assert_eq!(
                            self.plan.chunk, 1,
                            "identity is compared only at the first real Bind, before reads/drop"
                        );
                        self.await_probe.first_bind = None;
                        self.tx.send(S10Witness::AwaitBound(ticket)).unwrap();
                    }
                }
            }
            if let Some(source) = clone_source {
                let before = before.as_ref().expect("fresh clone source census");
                assert!(
                    actual.env.contains(&source),
                    "real clone leaves its complete source unchanged"
                );
                if self.faulted
                    && matches!(self.plan.fault, S10Fault::CloneCount | S10Fault::CloneBytes)
                {
                    let diagnostic = if self.plan.fault == S10Fault::CloneCount {
                        "FrameLimit"
                    } else {
                        "StorageLimit"
                    };
                    assert!(
                        matches!(&machine.pending, Some(Err(error)) if error.message.contains(diagnostic))
                    );
                    assert_eq!(actual.owners, before.owners);
                    assert_eq!(actual.clone_bytes, before.clone_bytes);
                    assert_eq!(
                        machine.metrics.peak_result_wrapper_capacity,
                        before.owners.len()
                    );
                    assert_eq!(
                        machine.metrics.peak_cloned_string_payload_bytes,
                        before.clone_bytes
                    );
                    self.rejected_source = true;
                } else {
                    let cloned = s11_measured_owner(
                        machine.values.as_slice().last().unwrap(),
                        self.proof.payload_text,
                        self.proof.task_errors,
                    )
                    .unwrap();
                    assert!(actual.owners.contains(&cloned));
                    s9_assert_distinct(source, cloned);
                    self.proof.final_clone = Some(cloned);
                    self.distinct_clones += 1;
                    s10_budget_totals(machine, &actual, self.input);
                    self.checked_budgets += 1;
                    if self.plan.fault == S10Fault::NativeClone && !self.faulted {
                        self.faulted = true;
                        let first = RuntimeTermination {
                            reason: TerminationReason::TimedOut,
                            cleanup_deadline: Instant::now() + HTTP_HANDLER_CLEANUP_GRACE,
                        };
                        let shorter = RuntimeTermination {
                            reason: TerminationReason::Cancelled,
                            cleanup_deadline: first
                                .cleanup_deadline
                                .checked_sub(Duration::from_millis(1))
                                .unwrap(),
                        };
                        let _cleanup = CleanupGuard::enter(first);
                        drop(CleanupGuard::enter(shorter));
                        let observed = current_cleanup_context().unwrap();
                        assert_eq!(observed.reason, first.reason);
                        assert_eq!(observed.cleanup_deadline, shorter.cleanup_deadline);
                        self.tx
                            .send(S10Witness::Native(Box::new(S10Native {
                                task: self.inner.task_id,
                                thread: self.thread,
                                source,
                                clone: cloned,
                                first,
                                observed,
                            })))
                            .unwrap();
                        panic!("S10 actual claimant native clone abandonment");
                    }
                }
            }
            let saved = machine.finally.frames.as_slice().iter().filter(|frame|
                matches!(&frame.primary, Some(Ok(Flow::Return { value, .. } | Flow::Fail { value, .. }))
                    if matches!(value, Value::Result { .. }))).count();
            if saved > self.max_saved {
                s10_budget_totals(machine, &actual, self.input);
                self.checked_budgets += 1;
                self.max_saved = saved;
            }
            self.tx
                .send(S10Witness::Progress {
                    dispatches: self.dispatches,
                    depth: machine.calls.len(),
                })
                .unwrap();
            if self.plan.pause == S10Pause::Running && !self.paused && !actual.env.is_empty() {
                self.pause(S10Pause::Running, None);
            }
        } else if let ControlFlow::Break(outcome) = &result {
            let outgoing = outcome.as_ref().ok().and_then(|value| {
                s11_measured_owner(value, self.proof.payload_text, self.proof.task_errors)
            });
            if let Some(actual) = outgoing {
                if chunk == 1 {
                    assert!(
                        self.proof.live.contains(&actual),
                        "outgoing owner was observed before real teardown"
                    );
                    if self.proof.final_clone.is_some() {
                        assert_eq!(self.proof.final_clone, Some(actual));
                    }
                }
            }
            s9_drained(machine);
            assert_eq!(self.inner.interpreter.call_depth, 0);
            let report = S10Finished {
                thread: self.thread,
                task: self.inner.task_id,
                calls: machine.metrics.entered_calls,
                retired: machine.metrics.retired_calls,
                peak_calls: machine.metrics.peak_calls,
                finally: machine.metrics.entered_finally_frames,
                retired_finally: machine.metrics.retired_finally_frames,
                peak_boxes: machine.metrics.peak_result_wrapper_capacity,
                peak_clones: machine.metrics.peak_cloned_string_payload_bytes,
                peak_errors: machine.metrics.peak_error_leaf_capacity,
                observed_peak_boxes: self.proof.peak_live,
                observed_peak_clones: self.proof.peak_clone_bytes,
                distinct_clones: self.distinct_clones,
                checked_budgets: self.checked_budgets,
                max_saved: self.max_saved,
                original_retired: !self.proof.original_live,
                depth_512: self.depth_512,
                outgoing,
                rejected_source: self.rejected_source,
            };
            self.tx
                .send(S10Witness::Finished(Box::new(report)))
                .unwrap();
            if self.plan.pause == S10Pause::Produced && !self.paused {
                self.pause(S10Pause::Produced, outgoing);
            }
        }
        result
    }

    fn waiting(&mut self) -> Option<&mut crate::runtime::task::AwaitTicket> {
        self.inner.waiting()
    }

    fn cleanup_wait_once(&mut self) {
        self.inner.cleanup_wait_once();
    }
}

fn s10_spawn(
    runtime: &TaskRuntime,
    source: &str,
    args: Vec<Value>,
    buffers: Vec<Buffer>,
    plan: S10Plan,
) -> (
    crate::runtime::task::TaskHandle,
    mpsc::Receiver<S10Witness>,
    mpsc::Sender<()>,
) {
    let program = Parser::new(
        Lexer::new(&format!("{source}\nfn main() {{}}"))
            .tokenize()
            .unwrap(),
    )
    .parse_program()
    .unwrap();
    crate::checker::Checker::new().check(&program).unwrap();
    let mut producer = production_interpreter(source);
    producer.task_runtime = Some(runtime.clone());
    let function = producer.functions["Entry"].clone();
    let code = plan.from_code.then(|| {
        CodeHandle::compile_claimed_async_owned_result_string_entry(
            &producer.functions,
            &function,
            Limits::default(),
        )
        .unwrap()
    });
    let child_runtime = runtime.clone();
    let (tx, rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let task = runtime
        .spawn_runner_deferred(|| {
            let template = producer.template();
            move || {
                assert!(current_cleanup_context().is_none());
                assert!(crate::runtime::task::current_execution_termination().is_none());
                let mut host = Interpreter::from_template(template, child_runtime);
                host.async_execution = true;
                let inner = if let Some(code) = code {
                    let function_id = code.find_function("Entry").unwrap();
                    ClaimedNamedTask::from_code(host, code, function_id, args, Span::default())
                } else {
                    ClaimedNamedTask::new(host, function, args, Span::default())
                };
                if let Some((_, args)) = &inner.legacy {
                    let strings = args
                        .iter()
                        .filter_map(|value| match value {
                            Value::String(text) => Some(text),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(strings.len(), buffers.len());
                    for (actual, original) in strings.iter().zip(&buffers) {
                        assert_eq!(
                            (actual.as_ptr() as usize, actual.capacity()),
                            (original.pointer, original.capacity)
                        );
                        assert_eq!(actual.as_str(), original.text);
                    }
                }
                assert_eq!(inner.interpreter.steps, 0);
                assert!(
                    inner.outcome.is_none(),
                    "valid fixture construction has no pending failure"
                );
                if !plan.selection_only {
                    let machine = inner
                        .machine
                        .as_ref()
                        .expect("new local role selected before any dispatch");
                    assert!(inner.legacy.is_none());
                    assert!(machine.code.has_actual_string_result_locals());
                    assert!(machine.string_origin_authorized);
                    assert_eq!(machine.metrics.entered_calls, 1);
                    assert_eq!(machine.metrics.dispatches, 0);
                }
                tx.send(S10Witness::Entered {
                    task: inner.task_id,
                    thread: std::thread::current().id(),
                    selected: inner.machine.is_some(),
                    legacy: inner.legacy.is_some(),
                    steps: inner.interpreter.steps,
                    calls: inner
                        .machine
                        .as_ref()
                        .map_or(0, |machine| machine.metrics.entered_calls),
                })
                .unwrap();
                let input = buffers[0];
                let mut proof = S9Proof::new(input);
                proof.task_errors = plan.task_errors;
                proof.allow_await_ticket = plan.await_locals;
                if let Some(text) = plan.payload_text {
                    proof.payload_text = text;
                }
                S10Runner {
                    inner,
                    plan,
                    input,
                    proof,
                    tx,
                    release: Some(release_rx),
                    thread: std::thread::current().id(),
                    dispatches: 0,
                    distinct_clones: 0,
                    checked_budgets: 0,
                    max_saved: 0,
                    depth_512: false,
                    faulted: false,
                    paused: false,
                    rejected_source: false,
                    await_probe: S11Probe::default(),
                }
            }
        })
        .unwrap();
    (task, rx, release_tx)
}

fn s10_entered(
    task: &crate::runtime::task::TaskHandle,
    rx: &mpsc::Receiver<S10Witness>,
    selected: bool,
) -> std::thread::ThreadId {
    let S10Witness::Entered {
        task: id,
        thread,
        selected: actual,
        legacy,
        steps,
        calls,
    } = rx.recv_timeout(S10_WAIT).unwrap()
    else {
        panic!("actual claimant constructor witness")
    };
    assert_ne!(id, 0);
    assert_eq!(id, task.id());
    assert_eq!(actual, selected);
    assert_eq!(legacy, !selected);
    assert_eq!(steps, 0);
    assert_eq!(calls, usize::from(selected));
    thread
}

fn s10_next(rx: &mpsc::Receiver<S10Witness>) -> S10Witness {
    let mut last = 0;
    for _ in 0..100_000 {
        match rx.recv_timeout(S10_WAIT).unwrap() {
            S10Witness::Progress { dispatches, depth } => {
                assert!(dispatches > last);
                assert!(depth <= MAX_CALL_DEPTH);
                last = dispatches;
            }
            event => return event,
        }
    }
    panic!("finite claimant message stream");
}

fn s10_finished(rx: &mpsc::Receiver<S10Witness>) -> S10Finished {
    let S10Witness::Finished(report) = s10_next(rx) else {
        panic!("actual completed claimant witness")
    };
    assert_eq!(report.calls, report.retired);
    assert_eq!(report.finally, report.retired_finally);
    *report
}

fn s10_retire(task: crate::runtime::task::TaskHandle, context: RuntimeTermination) {
    use crate::runtime::task::{RetirementBatchPoll, TaskRetirementBatch};
    let witness = task.retirement_witness();
    drop(task);
    let mut batch = TaskRetirementBatch::from_reserved(vec![witness], context);
    assert_eq!(batch.context(), context);
    let end = Instant::now() + S10_WAIT;
    loop {
        match batch.poll(Instant::now()) {
            RetirementBatchPoll::Retired => break,
            RetirementBatchPoll::TimedOut(actual) => {
                panic!("G1/G2 exceeded original D: {actual:?}")
            }
            RetirementBatchPoll::Pending { .. } => {
                assert!(Instant::now() < end);
                std::thread::yield_now();
            }
        }
    }
}

fn s10_close(runtime: &TaskRuntime) {
    runtime.cancel_all_and_wait(S10_WAIT).unwrap();
    let snapshot = runtime.snapshot().unwrap();
    assert_eq!(
        (
            snapshot.active_tasks,
            snapshot.queued_tasks,
            snapshot.registered_tasks,
            snapshot.wait_edges
        ),
        (0, 0, 0, 0)
    );
}

fn s10_completed(
    runtime: &TaskRuntime,
    source: &str,
    args: Vec<Value>,
    input: Buffer,
    plan: S10Plan,
) -> (S10Finished, Value) {
    let (task, rx, _) = s10_spawn(runtime, source, args, vec![input], plan);
    let thread = s10_entered(&task, &rx, true);
    let proof = s10_finished(&rx);
    assert_eq!(proof.thread, thread);
    assert_eq!(proof.task, task.id());
    let returned = s10_wait_result(&task).unwrap();
    // The ordinary await moves the sole published result. Compare against
    // the actual worker Break witness; no Diagnostic clone supplies identity.
    assert_eq!(s9_owner(&returned, input.text), proof.outgoing);
    s10_retire(task, RuntimeTermination::new(TerminationReason::Cancelled));
    (proof, returned)
}

#[test]
fn claimed_async_string_locals_construct_real_new_and_from_code_clones_across_chunks() {
    for error in [false, true] {
        let leaf = if error { "err(text)" } else { "ok(text)" };
        let direct = format!(
            "async fn Entry(text: str): str! {{ r: str! = {leaf}\nr\nr = r\nreturn r.clone() }}"
        );
        let helper = format!("fn Leaf(text: str, marker: int): str! {{ local: str! = {leaf}\nlocal\nreturn local.clone() }}\nasync fn Entry(text: str): str! {{ r: str! = Leaf(text, 7)\nr\nr = r\nreturn r.clone() }}");
        // The second actual typed local is never read. Its real clone and
        // teardown remain charged, and construction still needs S9 authority.
        let unread = format!("async fn Entry(text: str): str! {{ r: str! = {leaf}\nunused: str! = r.clone()\nreturn r.clone() }}");
        for source in [&direct, &helper, &unread] {
            for from_code in [false, true] {
                for chunk in [1, 7, 256] {
                    let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
                    let (text, buffer) = string_input(PAYLOAD, 4096);
                    let mut plan = S10Plan::normal(chunk);
                    plan.from_code = from_code;
                    let (proof, returned) =
                        s10_completed(&runtime, source, vec![text], buffer, plan);
                    let actual = s9_owner(&returned, PAYLOAD).unwrap();
                    assert_eq!(actual.error.is_some(), error);
                    assert_eq!(proof.outgoing.unwrap().error.is_some(), error);
                    assert_eq!(proof.calls, 1 + usize::from(source == &helper));
                    assert!(proof.peak_boxes >= 2);
                    if chunk == 1 {
                        assert!(proof.distinct_clones >= 2);
                        assert!(proof.checked_budgets >= 2);
                        assert_eq!(proof.peak_boxes, proof.observed_peak_boxes);
                        assert_eq!(proof.peak_clones, proof.observed_peak_clones);
                    }
                    if error {
                        assert_eq!(proof.peak_clones, 0);
                        assert!(proof.peak_errors >= 6);
                    } else {
                        assert!(proof.peak_clones >= PAYLOAD.len());
                    }
                    s10_close(&runtime);
                }
            }
        }
    }
}

#[test]
fn claimed_async_string_locals_real_terminal_relays_keep_512_first_513_and_same_worker_reuse() {
    std::thread::scope(|scope| {
        for error in [false, true] {
            scope.spawn(move || {
                let leaf = if error { "err(text)" } else { "ok(text)" };
                let entry = "async fn Entry(text: str, n: int): str! { r: str! = Relay(text, n)\nr\nr = r\nreturn r.clone() }";
                let self_source = format!("fn Relay(text: str, n: int): str! {{ if (n <= 0) {{ return {leaf} }}\nreturn Relay(text, n - 1) }}\n{entry}");
                let mutual_source = format!("fn Relay(text: str, n: int): str! {{ if (n <= 0) {{ return {leaf} }}\nreturn Other(text, n - 1) }}\nfn Other(text: str, n: int): str! {{ if (n <= 0) {{ return {leaf} }}\nreturn Relay(text, n - 1) }}\n{entry}");
                let fresh_source = format!("fn Relay(text: str, n: int): str! {{ return Bridge(text, n) }}\nfn Bridge(text: str, n: int): str! {{ if (n <= 0) {{ return {leaf} }}\nreturn Bridge(text, n - 1) }}\n{entry}");
                for (source, extra, chunk) in [(&self_source, 0, 1), (&mutual_source, 0, 7), (&fresh_source, 1, 256)] {
                    let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
                    let mut worker = None;
                    for n in [510 - extra, 511 - extra, 0] {
                        let (text, buffer) = string_input(PAYLOAD, 4096);
                        let mut plan = S10Plan::normal(chunk);
                        plan.deep = n != 0;
                        plan.from_code = n == 0;
                        let (task, rx, _) = s10_spawn(&runtime, source, vec![text, Value::Int(n)], vec![buffer], plan);
                        let actual_worker = s10_entered(&task, &rx, true);
                        if let Some(worker) = worker { assert_eq!(actual_worker, worker); }
                        worker = Some(actual_worker);
                        let proof = s10_finished(&rx);
                        assert_eq!(proof.thread, actual_worker);
                        let result = s10_wait_result(&task);
                        if n == 511 - extra {
                            assert_eq!(result.unwrap_err().code.as_deref(), Some("call_depth_exceeded"));
                            assert_eq!(proof.calls, MAX_CALL_DEPTH);
                            assert_eq!(proof.peak_calls, MAX_CALL_DEPTH);
                            assert!(proof.outgoing.is_none());
                        } else {
                            assert_eq!(s9_owner(&result.unwrap(), PAYLOAD).unwrap().error.is_some(), error);
                            assert_eq!(proof.calls, (n + 2 + extra) as usize);
                            assert_eq!(proof.outgoing.unwrap().error.is_some(), error);
                        }
                        if n != 0 {
                            assert!(proof.depth_512, "the exact real 512 checkpoint was exposed");
                            assert!(proof.checked_budgets != 0);
                        }
                        s10_retire(task, RuntimeTermination::new(TerminationReason::Cancelled));
                    }
                    s10_close(&runtime);
                }
            });
        }
    });
}

#[test]
fn claimed_async_string_local_clone_minimum_quotas_preserve_real_source_and_four_budgets() {
    for error in [false, true] {
        let leaf = if error { "err(text)" } else { "ok(text)" };
        let source = format!("async fn Entry(text: str): str! {{ r: str! = {leaf}\nr\nreturn r }}");
        let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
        let mut worker = None;
        for fault in [S10Fault::CloneCount, S10Fault::CloneBytes, S10Fault::None] {
            let (text, buffer) = string_input(PAYLOAD, 4096);
            let mut plan = S10Plan::normal(1);
            plan.fault = fault;
            let (task, rx, _) = s10_spawn(&runtime, &source, vec![text], vec![buffer], plan);
            let actual_worker = s10_entered(&task, &rx, true);
            if let Some(worker) = worker {
                assert_eq!(actual_worker, worker);
            }
            worker = Some(actual_worker);
            let proof = s10_finished(&rx);
            let result = s10_wait_result(&task);
            if fault == S10Fault::None {
                assert_eq!(
                    s9_owner(&result.unwrap(), PAYLOAD).unwrap().error.is_some(),
                    error
                );
                assert!(proof.distinct_clones >= 2);
                assert_eq!(proof.peak_boxes, proof.observed_peak_boxes);
                assert_eq!(proof.peak_clones, proof.observed_peak_clones);
            } else {
                let diagnostic = if fault == S10Fault::CloneCount {
                    "FrameLimit"
                } else {
                    "StorageLimit"
                };
                assert!(result.unwrap_err().message.contains(diagnostic));
                assert!(proof.rejected_source);
                assert_eq!(proof.peak_boxes, 1);
                assert_eq!(proof.peak_clones, 0);
                assert_eq!(proof.distinct_clones, 0);
                assert!(proof.outgoing.is_none());
            }
            assert!(proof.checked_budgets >= 2);
            s10_retire(task, RuntimeTermination::new(TerminationReason::Cancelled));
        }
        s10_close(&runtime);
    }
    // S9's unchanged sparse real ErrorObject clone regression also exercises
    // actual Map-capacity rejection and its buffer-only peak before a Box.
    // This claimant fixture does not fake a String::clone capacity to force
    // a path the current allocator does not take for these source Strings.
}

#[test]
fn claimed_async_string_local_origin_tombstone_counts_empty_and_reserved_empty_inputs() {
    const SOURCE: &str =
        "async fn Entry(text: str): str! { r: str! = ok(text)\nr = r\nr\nreturn r.clone() }";
    let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
    for (text, capacity) in [(PAYLOAD, 4096), ("", 0), ("", 4096)] {
        let (value, buffer) = string_input(text, capacity);
        let (proof, returned) =
            s10_completed(&runtime, SOURCE, vec![value], buffer, S10Plan::normal(1));
        assert!(
            proof.original_retired,
            "assignment destroyed the original credit permanently"
        );
        assert!(proof.distinct_clones >= 3);
        assert!(proof.checked_budgets >= 3);
        assert_eq!(proof.peak_clones, proof.observed_peak_clones);
        assert_eq!(
            s9_owner(&returned, text).unwrap().string.unwrap().len,
            text.len()
        );
        if text.is_empty() {
            assert_eq!(proof.peak_clones, 0);
        } else {
            assert!(proof.peak_clones >= text.len());
        }
    }
    s10_close(&runtime);
}

#[test]
fn claimed_async_string_local_scopes_q2_q3_and_protected_first_cause_keep_real_owners() {
    for error in [false, true] {
        let leaf = if error { "err(text)" } else { "ok(text)" };
        let scoped = format!("async fn Entry(text: str): str! {{ r: str! = {leaf}\nif (true) {{ inner: str! = r.clone()\ninner }}\nreturn r.clone() }}");
        let q2 = format!("async fn Entry(text: str): str! {{ r: str! = {leaf}\ntry {{ return r.clone() }} finally {{ r\nr = r\nreturn r.clone() }}\nreturn r.clone() }}");
        let q3 = format!("async fn Entry(text: str): str! {{ r: str! = {leaf}\ntry {{ return r.clone() }} finally {{ try {{ return r.clone() }} finally {{ r\nr = r\nreturn r.clone() }} }}\nreturn r.clone() }}");
        let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
        for (source, saved) in [(&scoped, 0), (&q2, 1), (&q3, 2)] {
            let (text, buffer) = string_input(PAYLOAD, 4096);
            let (proof, returned) =
                s10_completed(&runtime, source, vec![text], buffer, S10Plan::normal(1));
            assert_eq!(s9_owner(&returned, PAYLOAD).unwrap().error.is_some(), error);
            assert_eq!(proof.max_saved, saved);
            assert_eq!(proof.finally, saved);
            assert!(proof.distinct_clones > saved);
            assert_eq!(proof.peak_boxes, proof.observed_peak_boxes);
            assert_eq!(proof.peak_clones, proof.observed_peak_clones);
        }
        s10_close(&runtime);
    }
    const PROTECTED: &str = "async fn Entry(text: str): str! { r: str! = ok(text)\ntry { marker = 1\nmarker = 2 } finally { r\nr = r }\nreturn r }";
    let source = functions(PROTECTED);
    let Stmt::Try { body, .. } = &source["Entry"].body[1] else {
        panic!("actual try fixture")
    };
    let target = stmt_span(&body[1]);
    let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
    let (text, buffer) = string_input(PAYLOAD, 4096);
    let mut plan = S10Plan::normal(1);
    plan.pause = S10Pause::Protected(target);
    let (task, rx, release) = s10_spawn(&runtime, PROTECTED, vec![text], vec![buffer], plan);
    s10_entered(&task, &rx, true);
    assert!(
        matches!(s10_next(&rx), S10Witness::Paused { stage: S10Pause::Protected(actual), owner: None } if actual == target)
    );
    let first = RuntimeTermination::new(TerminationReason::Cancelled);
    let shorter = RuntimeTermination {
        reason: TerminationReason::TimedOut,
        cleanup_deadline: first
            .cleanup_deadline
            .checked_sub(Duration::from_millis(1))
            .unwrap(),
    };
    assert!(task.request_cancel_with(first));
    assert!(!task.request_cancel_with(shorter));
    let expected = RuntimeTermination {
        reason: first.reason,
        cleanup_deadline: shorter.cleanup_deadline,
    };
    release.send(()).unwrap();
    let proof = s10_finished(&rx);
    assert_eq!(
        s10_wait_result(&task).unwrap_err().runtime_termination(),
        Some(expected)
    );
    assert_eq!(proof.finally, 1);
    assert_eq!(proof.retired_finally, 1);
    assert!(
        proof.distinct_clones >= 2,
        "protected finally genuinely read and assigned r"
    );
    assert!(proof.peak_clones != 0);
    assert!(proof.outgoing.is_none());
    s10_retire(task, expected);
    s10_close(&runtime);
}

#[test]
fn claimed_async_string_local_queued_running_produced_cancel_and_owner_release_retire_g1_g2() {
    const SOURCE: &str =
        "async fn Entry(text: str): str! { r: str! = ok(text)\nr\nr = r\nreturn r.clone() }";
    let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
    let (ready_tx, ready_rx) = mpsc::channel();
    let (held_tx, held_rx) = mpsc::channel();
    let held = runtime
        .spawn(move || {
            ready_tx.send(()).unwrap();
            held_rx.recv_timeout(S10_WAIT).unwrap();
            Ok(Value::Null)
        })
        .unwrap();
    ready_rx.recv_timeout(S10_WAIT).unwrap();
    let (text, buffer) = string_input(PAYLOAD, 4096);
    let (task, rx, _) = s10_spawn(
        &runtime,
        SOURCE,
        vec![text],
        vec![buffer],
        S10Plan::normal(1),
    );
    assert_eq!(runtime.snapshot().unwrap().queued_tasks, 1);
    let first = RuntimeTermination::new(TerminationReason::TimedOut);
    assert!(task.request_cancel_with(first));
    assert!(
        matches!(
            rx.recv_timeout(S10_WAIT),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ),
        "queued factory and original String are released without construction"
    );
    assert_eq!(
        s10_wait_result(&task).unwrap_err().runtime_termination(),
        Some(first)
    );
    s10_retire(task, first);
    assert_eq!(runtime.snapshot().unwrap().queued_tasks, 0);
    held_tx.send(()).unwrap();
    assert_eq!(s10_wait_result(&held).unwrap(), Value::Null);
    s10_retire(held, RuntimeTermination::new(TerminationReason::Cancelled));
    for stage in [S10Pause::Running, S10Pause::Produced] {
        let (text, buffer) = string_input(PAYLOAD, 4096);
        let mut plan = S10Plan::normal(1);
        plan.pause = stage;
        let (task, rx, release) = s10_spawn(&runtime, SOURCE, vec![text], vec![buffer], plan);
        s10_entered(&task, &rx, true);
        let completed = if stage == S10Pause::Produced {
            Some(s10_finished(&rx))
        } else {
            None
        };
        let S10Witness::Paused {
            stage: actual,
            owner,
        } = s10_next(&rx)
        else {
            panic!("real cancel checkpoint")
        };
        assert_eq!(actual, stage);
        if let Some(proof) = completed {
            assert_eq!(owner.as_deref().copied(), proof.outgoing);
            assert!(owner.is_some());
        } else {
            assert!(owner.is_none());
        }
        let first = RuntimeTermination::new(TerminationReason::TimedOut);
        let shorter = RuntimeTermination {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: first
                .cleanup_deadline
                .checked_sub(Duration::from_millis(1))
                .unwrap(),
        };
        assert!(task.request_cancel_with(first));
        assert!(!task.request_cancel_with(shorter));
        let expected = RuntimeTermination {
            reason: first.reason,
            cleanup_deadline: shorter.cleanup_deadline,
        };
        release.send(()).unwrap();
        if stage == S10Pause::Running {
            let proof = s10_finished(&rx);
            assert!(proof.outgoing.is_none());
        }
        assert_eq!(
            s10_wait_result(&task).unwrap_err().runtime_termination(),
            Some(expected)
        );
        s10_retire(task, expected);
    }
    let (text, buffer) = string_input(PAYLOAD, 4096);
    let mut plan = S10Plan::normal(1);
    plan.pause = S10Pause::Produced;
    let (task, rx, release) = s10_spawn(&runtime, SOURCE, vec![text], vec![buffer], plan);
    s10_entered(&task, &rx, true);
    let proof = s10_finished(&rx);
    assert!(proof.outgoing.is_some());
    assert!(matches!(
        s10_next(&rx),
        S10Witness::Paused {
            stage: S10Pause::Produced,
            owner: Some(_)
        }
    ));
    let witness = task.retirement_witness();
    drop(task);
    let context = RuntimeTermination::new(TerminationReason::Cancelled);
    let mut batch =
        crate::runtime::task::TaskRetirementBatch::from_reserved(vec![witness], context);
    assert_eq!(batch.context(), context);
    assert!(
        matches!(
            batch.poll(Instant::now()),
            crate::runtime::task::RetirementBatchPoll::Pending { .. }
        ),
        "real uncommitted incoming owner and running queue node still require both receipts"
    );
    release.send(()).unwrap();
    let end = Instant::now() + S10_WAIT;
    loop {
        match batch.poll(Instant::now()) {
            crate::runtime::task::RetirementBatchPoll::Retired => break,
            crate::runtime::task::RetirementBatchPoll::TimedOut(actual) => {
                panic!("original retirement D expired: {actual:?}")
            }
            crate::runtime::task::RetirementBatchPoll::Pending { .. } => {
                assert!(Instant::now() < end);
                std::thread::yield_now();
            }
        }
    }
    s10_close(&runtime);
}

#[test]
fn claimed_async_string_local_actual_native_clone_drop_restores_tls_and_same_worker_health() {
    for error in [false, true] {
        let leaf = if error { "err(text)" } else { "ok(text)" };
        let source =
            format!("async fn Entry(text: str): str! {{ r: str! = {leaf}\nr\nreturn r.clone() }}");
        let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
        let (text, buffer) = string_input(PAYLOAD, 4096);
        let mut plan = S10Plan::normal(1);
        plan.fault = S10Fault::NativeClone;
        let (task, rx, _) = s10_spawn(&runtime, &source, vec![text], vec![buffer], plan);
        let worker = s10_entered(&task, &rx, true);
        let S10Witness::Native(native) = s10_next(&rx) else {
            panic!("real source and clone before native unwind")
        };
        let S10Native {
            task: id,
            thread,
            source: actual_source,
            clone,
            first,
            observed,
        } = *native;
        assert_eq!(id, task.id());
        assert_eq!(thread, worker);
        s9_assert_distinct(actual_source, clone);
        assert_eq!(observed.reason, first.reason);
        assert!(observed.cleanup_deadline < first.cleanup_deadline);
        let Value::Result { ok: false, value } = s10_wait_result(&task).unwrap() else {
            panic!("existing task panic classification")
        };
        let Value::Object(fields) = *value else {
            panic!("real driver panic object")
        };
        assert!(matches!(fields.get("code"), Some(Value::String(code)) if code == "panic"));
        s10_retire(task, RuntimeTermination::new(TerminationReason::Cancelled));
        let (text, buffer) = string_input(PAYLOAD, 4096);
        let (proof, returned) =
            s10_completed(&runtime, &source, vec![text], buffer, S10Plan::normal(1));
        assert_eq!(proof.thread, worker);
        assert_ne!(proof.task, id);
        assert_eq!(s9_owner(&returned, PAYLOAD).unwrap().error.is_some(), error);
        s10_close(&runtime);
    }
}

#[test]
fn claimed_async_string_local_whole_graph_noawait_boundaries_preserve_original_factory_inputs() {
    for source in [
        "async fn Child(text: str): str! { return ok(text) }\nasync fn Entry(text: str): str! { r: str! = await Child(text)\nreturn r }",
        "async fn Child(text: str): str! { return ok(text) }\nasync fn Entry(text: str): str! { r: str! = ok(text)\nreturn r.clone()\nlater = await Child(\"later\") }",
        "async fn Child(text: str): str! { return ok(text) }\nasync fn Entry(text: str): str! { task = Child(text)\nr: str! = err(\"later\")\nreturn r }",
        "fn Pass(value: str!): str! { return value }\nasync fn Entry(text: str): str! { r: str! = ok(text)\nreturn Pass(r) }",
        "async fn Entry(text: str): str! { r: str! = ok(text.clone())\nreturn r }",
        "async fn Entry(text: str): str! { r: str! = ok(text)\nreturn r.clone().clone() }",
        "async fn Entry(text: str): str! { r: str! = ok(text)\nreturn ok(r?) }",
        "async fn Entry(text: str): str! { r: str! = ok(text)\ntry { marker = 1 } catch (e) { marker = 2 }\nreturn r }",
    ] {
        let source_functions = functions(source);
        let entry = &source_functions["Entry"];
        assert!(!code::eligible_owned_named(&source_functions, entry, Limits::default()));
        let new_await_local = source == "async fn Child(text: str): str! { return ok(text) }\nasync fn Entry(text: str): str! { r: str! = await Child(text)\nreturn r }";
        let admission = CodeHandle::compile_claimed_async_owned_result_string_entry(&source_functions, entry, Limits::default());
        if new_await_local {
            assert!(admission.unwrap().has_actual_string_result_locals(),
                "S11 admits this precise awaited typed local");
        } else {
            assert!(matches!(admission, Err(AdmissionError::Unsupported(_))), "{source}");
        }
        let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
        let (text, buffer) = string_input(PAYLOAD, 4096);
        let mut plan = S10Plan::normal(1);
        plan.selection_only = true;
        let (task, rx, _) = s10_spawn(&runtime, source, vec![text], vec![buffer], plan);
        s10_entered(&task, &rx, new_await_local);
        assert_eq!(s10_wait_result(&task).unwrap(), Value::Null);
        s10_retire(task, RuntimeTermination::new(TerminationReason::Cancelled));
        s10_close(&runtime);
    }
    const MULTI: &str =
        "async fn Entry(text: str, other: str): str! { r: str! = ok(text)\nreturn r }";
    let functions = functions(MULTI);
    assert!(matches!(
        CodeHandle::compile_claimed_async_owned_result_string_entry(
            &functions,
            &functions["Entry"],
            Limits::default()
        ),
        Err(AdmissionError::Unsupported(_))
    ));
    let runtime = TaskRuntime::with_limits(1, 8, 0, 0, 8);
    let (text, first) = string_input(PAYLOAD, 4096);
    let (other, second) = string_input("unmoved", 8192);
    let mut plan = S10Plan::normal(1);
    plan.selection_only = true;
    let (task, rx, _) = s10_spawn(
        &runtime,
        MULTI,
        vec![text, other],
        vec![first, second],
        plan,
    );
    s10_entered(&task, &rx, false);
    assert_eq!(s10_wait_result(&task).unwrap(), Value::Null);
    s10_retire(task, RuntimeTermination::new(TerminationReason::Cancelled));
    s10_close(&runtime);
}

// S11 adds only real ticket observation to the existing physical-owner oracle.
// Production dispatch and all task lifecycle operations remain the old runner's.
#[derive(Default)]
struct S11Probe {
    yielded_span: Option<SpanId>,
    ready_seen: bool,
    waiting_seen: bool,
    first_bind: Option<S9Owner>,
    child_seen: bool,
}

#[derive(Debug)]
struct S11Ready {
    owner: S9Owner,
    input_capacity: usize,
    current_clone_bytes: usize,
    cached_string_bytes: usize,
    stable_clone_bytes: usize,
    count: usize,
    bytes: usize,
    origin_live: bool,
}

fn s11_box_error_charge(owner: S9Owner) -> AwaitPayloadCharge {
    let error = owner.error;
    let box_bytes = std::mem::size_of::<Value>();
    AwaitPayloadCharge {
        count: 1 + error.map_or(0, |error| error.count),
        bytes: box_bytes + error.map_or(0, |error| error.bytes),
        box_count: 1,
        box_bytes,
        error_leaf_count: error.map_or(0, |error| error.count),
        error_leaf_bytes: error.map_or(0, |error| error.bytes),
    }
}

fn s11_plan(chunk: usize, from_code: bool) -> S10Plan {
    let mut plan = S10Plan::normal(chunk);
    plan.await_locals = true;
    plan.from_code = from_code;
    plan
}

fn s11_receipt(
    runtime: &TaskRuntime,
    source: &str,
    args: Vec<Value>,
    input: Buffer,
    plan: S10Plan,
) -> (S10Finished, KuResult<Value>, Vec<S11Ready>, usize) {
    let (task, rx, _) = s10_spawn(runtime, source, args, vec![input], plan);
    let worker = s10_entered(&task, &rx, true);
    let mut ready = Vec::new();
    let mut bound = 0;
    let mut children = Vec::new();
    let mut steps = 0;
    let proof = loop {
        steps += 1;
        assert!(
            steps <= 100_000,
            "finite actual stable-dispatch receipt stream"
        );
        match rx.recv_timeout(S10_WAIT).unwrap() {
            S10Witness::Finished(proof) => break *proof,
            S10Witness::Progress { dispatches, depth } => {
                assert!(dispatches > 0 && depth <= MAX_CALL_DEPTH);
            }
            S10Witness::AwaitWaiting => {}
            S10Witness::AwaitChild(witness) => children.push(witness.0),
            S10Witness::AwaitReady(receipt) => ready.push(*receipt),
            S10Witness::AwaitBound(owner) => {
                assert!(ready.iter().any(|receipt| receipt.owner == owner));
                bound += 1;
            }
            other => panic!("unexpected normal actual await claimant event: {other:?}"),
        }
    };
    assert_eq!(proof.thread, worker);
    assert_eq!(proof.task, task.id());
    assert_eq!(proof.calls, proof.retired);
    assert_eq!(proof.finally, proof.retired_finally);
    let result = s10_wait_result(&task);
    let payload_text = plan.payload_text.unwrap_or(input.text);
    assert_eq!(
        result.as_ref().ok().and_then(|value| s11_measured_owner(
            value,
            payload_text,
            plan.task_errors
        )),
        proof.outgoing,
        "ordinary consuming result returns the actual final Box and backing"
    );
    if plan.chunk == 1 {
        assert_eq!(
            bound,
            ready.len(),
            "each actual ticket reaches the first real parent Env binding"
        );
    } else {
        assert_eq!(
            bound, 0,
            "coarse chunks compare no possibly retired historical addresses"
        );
    }
    assert_eq!(
        children.len(),
        1,
        "the fixture observes the actual child Task operand once"
    );
    let context = RuntimeTermination::new(TerminationReason::Cancelled);
    s10_retire(task, context);
    s11_retire_children(children, context);
    (proof, result, ready, bound)
}

#[test]
fn claimed_async_await_string_local_real_new_from_code_ready_and_clone_census_across_chunks() {
    for chunk in [1, 7, 256] {
        for from_code in [false, true] {
            for error in [false, true] {
                let source = format!(
                    "async fn Child(text: str): str! {{ r: str! = {}(text)\nreturn r.clone() }}\n\
                     async fn Entry(text: str): str! {{ r: str! = await Child(text)\nr\nr = r.clone()\nreturn r.clone() }}",
                    if error { "err" } else { "ok" });
                let runtime = TaskRuntime::with_limits(1, 16, 0, 0, 16);
                let mut same_worker = None;
                for _ in 0..2 {
                    let (text, input) = string_input(PAYLOAD, 4096);
                    let (proof, value, ready, bound) = s11_completed(
                        &runtime,
                        &source,
                        vec![text],
                        input,
                        s11_plan(chunk, from_code),
                    );
                    if let Some(worker) = same_worker {
                        assert_eq!(worker, proof.thread);
                    }
                    same_worker = Some(proof.thread);
                    assert_eq!(ready.len(), 1);
                    assert_eq!(ready[0].input_capacity, input.capacity);
                    assert!(
                        !ready[0].origin_live,
                        "the parent input credit was permanently tombstoned on child transfer"
                    );
                    assert_eq!(
                        ready[0].cached_string_bytes, 0,
                        "driver Ready occurred before Machine snapshot refresh"
                    );
                    assert_eq!(ready[0].stable_clone_bytes, 0);
                    assert_eq!(
                        ready[0].current_clone_bytes,
                        ready[0].owner.string.map_or(0, |text| text.capacity)
                    );
                    assert!(ready[0].count >= s11_box_error_charge(ready[0].owner).count);
                    assert!(
                        ready[0].bytes
                            >= input.capacity
                                + s11_box_error_charge(ready[0].owner).bytes
                                + ready[0].current_clone_bytes
                    );
                    assert_eq!(matches!(value, Value::Result { ok: false, .. }), error);
                    assert_eq!(ready[0].owner.error.is_some(), error);
                    if !error {
                        let child = ready[0].owner.string.unwrap();
                        assert_eq!(child.len, PAYLOAD.len());
                        assert!(child.capacity < input.capacity,
                            "the actual child Result clone has smaller capacity than the parent pool");
                    }
                    if chunk == 1 {
                        assert_eq!(bound, 1);
                        assert!(proof.distinct_clones >= 3);
                        assert!(proof.checked_budgets >= 4);
                        assert_eq!(proof.peak_boxes, proof.observed_peak_boxes);
                        assert_eq!(proof.peak_clones, proof.observed_peak_clones);
                    }
                    assert!(proof.original_retired);
                }
                s10_close(&runtime);
            }
        }
    }
}

#[test]
fn claimed_async_await_string_local_original_buffer_and_empty_capacity_are_fully_charged() {
    const SOURCE: &str = "async fn Child(text: str): str! { return ok(text) }\n\
        async fn Entry(text: str): str! { r: str! = await Child(text)\nreturn r.clone() }";
    let runtime = TaskRuntime::with_limits(1, 16, 0, 0, 16);
    for (text, capacity) in [("", 0), ("", 4096), (PAYLOAD, 4096)] {
        let (value, input) = string_input(text, capacity);
        let (proof, _, ready, bound) =
            s11_completed(&runtime, SOURCE, vec![value], input, s11_plan(1, false));
        assert_eq!(ready.len(), 1);
        assert_eq!(bound, 1);
        let actual = ready[0].owner.string.unwrap();
        assert_eq!(
            (actual.pointer, actual.capacity),
            (input.pointer, input.capacity)
        );
        assert_eq!(
            ready[0].current_clone_bytes, input.capacity,
            "an original buffer returned from the child receives the full additional await fee"
        );
        assert!(
            !ready[0].origin_live,
            "input pointer equality after transfer never revives origin authority"
        );
        assert!(proof.original_retired);
    }
    s10_close(&runtime);
}

#[test]
fn claimed_async_await_string_local_whole_graph_literal_boundary_preserves_original_factory_inputs()
{
    // This checker-legal literal remains whole Unsupported. Construction must
    // retain the actual original inputs and execute no body or legacy step.
    const SOURCE: &str = "async fn Child(text: str): str! { return ok(text) }\n\
        async fn Entry(text: str): str! { r: str! = await Child(\"中é\0kept\")\nreturn r.clone() }";
    let runtime = TaskRuntime::with_limits(1, 16, 0, 0, 16);
    let source = functions(SOURCE);
    let before = source.clone();
    assert!(matches!(
        CodeHandle::compile_claimed_async_owned_result_string_entry(
            &source,
            &source["Entry"],
            Limits::default(),
        ),
        Err(AdmissionError::Unsupported(_))
    ));
    assert_eq!(source, before);
    let (value, input) = string_input("", 0);
    let mut plan = s11_plan(1, false);
    plan.selection_only = true;
    let (task, rx, _) = s10_spawn(&runtime, SOURCE, vec![value], vec![input], plan);
    s10_entered(&task, &rx, false);
    assert!(matches!(s10_wait_result(&task), Ok(Value::Null)));
    s10_retire(task, RuntimeTermination::new(TerminationReason::Cancelled));
    s10_close(&runtime);
}

fn s11_copy_owner(candidate: &Value) -> Option<S9Owner> {
    // Immutable primitive metadata only; safe to call under the actual result
    // mutex. No assertion, clone, allocation, callback or owner drop occurs.
    let Value::Result { ok, value } = candidate else {
        return None;
    };
    let boxed = value.as_ref() as *const Value as usize;
    if *ok {
        let Value::String(text) = value.as_ref() else {
            return None;
        };
        Some(S9Owner {
            boxed,
            string: Some(S9String {
                pointer: text.as_ptr() as usize,
                len: text.len(),
                capacity: text.capacity(),
            }),
            error: None,
        })
    } else {
        Some(S9Owner {
            boxed,
            string: None,
            error: Some(error_payload(candidate)?),
        })
    }
}

fn s11_slot_owner(task: &crate::runtime::task::TaskHandle) -> Option<S9Owner> {
    task.test_with_result_slot(|slot| {
        slot.and_then(|result| result.as_ref().ok())
            .and_then(s11_copy_owner)
    })
    .unwrap()
}

fn s11_rejected_ordinary_slot_keeps_claim_and_payload(
    task: &crate::runtime::task::TaskHandle,
    original: S9Owner,
) {
    assert_eq!(
        s11_slot_owner(task),
        Some(original),
        "policy rejection and ticket release preserve the original real slot"
    );
    let repeated = s10_wait_result(task).unwrap();
    let repeated_owner = s11_copy_owner(&repeated).unwrap();
    assert_ne!(
        repeated_owner.boxed, original.boxed,
        "both actual owners are live; the second await returns a separate diagnostic"
    );
    let Value::Result { ok: false, value } = &repeated else {
        panic!("ordinary await claim remains consumed after payload admission rejection");
    };
    let Value::Object(fields) = value.as_ref() else {
        panic!("actual second ordinary await has the existing task ErrorObject");
    };
    assert!(matches!(fields.get("domain"), Some(Value::String(value)) if value == "task"));
    assert!(matches!(fields.get("code"), Some(Value::String(value)) if value == "already_awaited"));
    assert!(matches!(fields.get("message"), Some(Value::String(value))
        if value == &format!("task {} has already been awaited", task.id())));
    assert_eq!(
        s11_slot_owner(task),
        Some(original),
        "the second ordinary await never consumes or clones the retained payload"
    );
}

fn s11_wait_published(task: &crate::runtime::task::TaskHandle) {
    let deadline = Instant::now() + S10_WAIT;
    while !task.test_with_result_slot(|slot| slot.is_some()).unwrap() {
        assert!(
            Instant::now() < deadline,
            "absolute bounded real slot publication"
        );
        std::thread::yield_now();
    }
}

#[test]
fn claimed_async_await_string_local_compound_quotas_admit_once_or_leave_real_child_slot_intact() {
    use crate::runtime::task::{AwaitStringPayloadPolicy, AwaitTicket};
    for error in [false, true] {
        let source = format!(
            "async fn Entry(text: str): str! {{ r: str! = {}(text)\nreturn r.clone() }}",
            if error { "err" } else { "ok" }
        );
        let runtime = TaskRuntime::with_limits(1, 16, 0, 0, 16);
        let mut worker = None;
        for (reason, zero_input_capacity) in [Some("FrameLimit"), Some("StorageLimit"), None]
            .into_iter()
            .flat_map(|reason| [false, true].map(|zero| (reason, zero)))
        {
            let (value, input) = string_input(PAYLOAD, 4096);
            let (child, rx, _) = s10_spawn(
                &runtime,
                &source,
                vec![value],
                vec![input],
                S10Plan::normal(1),
            );
            let current = s10_entered(&child, &rx, true);
            if let Some(old) = worker {
                assert_eq!(old, current);
            }
            worker = Some(current);
            let proof = s10_finished(&rx);
            assert!(
                proof.distinct_clones >= 1,
                "candidate was observed simultaneously with the real child source"
            );
            s11_wait_published(&child);
            let original = s11_slot_owner(&child).unwrap();
            assert_eq!(Some(original), proof.outgoing);
            let shape = s11_box_error_charge(original);
            let string_bytes = original.string.map_or(0, |string| string.capacity);
            let full_bytes = shape.bytes + string_bytes;
            let policy_input_capacity = if zero_input_capacity {
                0
            } else {
                input.capacity
            };
            if !error && zero_input_capacity {
                assert!(string_bytes > policy_input_capacity);
            }
            if error {
                assert_eq!(string_bytes, 0);
            }
            let mut ticket = AwaitTicket::begin_with_string_payload_policy(
                child.clone(),
                AwaitStringPayloadPolicy::new(AwaitPayloadPolicy {
                    max_count: shape.count - usize::from(reason == Some("FrameLimit")),
                    max_bytes: full_bytes - usize::from(reason == Some("StorageLimit")),
                    // Both larger original reservation and zero reservation variants
                    // must charge the actual independently produced child payload.
                    input_string_capacity: policy_input_capacity,
                    span: Span::default(),
                }),
            );
            s11_poll_ready(&mut ticket);
            if let Some(reason) = reason {
                assert_eq!(
                    s11_slot_owner(&child),
                    Some(original),
                    "preTake rejection retains the exact original real slot payload"
                );
                assert_eq!(
                    ticket.retained_payload_charge(),
                    AwaitPayloadCharge::default()
                );
                assert_eq!(ticket.retained_string_payload_charge().bytes, 0);
                assert!(ticket
                    .finish_with(|_| {})
                    .unwrap_err()
                    .message
                    .contains(reason));
                s11_rejected_ordinary_slot_keeps_claim_and_payload(&child, original);
            } else {
                assert!(
                    s11_slot_owner(&child).is_none(),
                    "successful Ordinary observation takes the sole real payload exactly once"
                );
                assert_eq!(
                    s11_copy_owner(ticket.test_outcome().unwrap().as_ref().unwrap()),
                    Some(original)
                );
                assert_eq!(
                    ticket.retained_payload_charge(),
                    shape,
                    "old Box/error charge excludes separate String bytes"
                );
                assert_eq!(ticket.retained_string_payload_charge().bytes, string_bytes);
                let mut observed = None;
                let returned = ticket
                    .finish_with(|outcome| {
                        observed = outcome.as_ref().ok().and_then(s11_copy_owner);
                    })
                    .unwrap();
                assert_eq!(observed, Some(original));
                assert_eq!(
                    s9_owner(&returned, PAYLOAD),
                    Some(original),
                    "observer and ordinary transfer keep the actual Box/backing intact"
                );
            }
            s10_retire(child, RuntimeTermination::new(TerminationReason::Cancelled));
        }
        s10_close(&runtime);
    }
}

#[test]
fn claimed_async_await_string_local_actual_finish_observer_native_unwind_and_same_worker_reuse() {
    use crate::runtime::task::{AwaitStringPayloadPolicy, AwaitTicket};
    const SOURCE: &str = "async fn Entry(text: str): str! { r: str! = ok(text)\nreturn r.clone() }";
    let runtime = TaskRuntime::with_limits(1, 16, 0, 0, 16);
    let (value, input) = string_input(PAYLOAD, 4096);
    let (child, rx, _) = s10_spawn(
        &runtime,
        SOURCE,
        vec![value],
        vec![input],
        S10Plan::normal(1),
    );
    let worker = s10_entered(&child, &rx, true);
    let proof = s10_finished(&rx);
    s11_wait_published(&child);
    let owner = s11_slot_owner(&child).unwrap();
    assert_eq!(Some(owner), proof.outgoing);
    let shape = s11_box_error_charge(owner);
    let string_bytes = owner.string.unwrap().capacity;
    let child_for_actor = child.clone();
    let (observer_tx, observer_rx) = mpsc::sync_channel(1);
    let actor = runtime
        .spawn(move || {
            assert_eq!(std::thread::current().id(), worker);
            let mut ticket = AwaitTicket::begin_with_string_payload_policy(
                child_for_actor,
                AwaitStringPayloadPolicy::new(AwaitPayloadPolicy {
                    max_count: shape.count,
                    max_bytes: shape.bytes + string_bytes,
                    input_string_capacity: 0,
                    span: Span::default(),
                }),
            );
            s11_poll_ready(&mut ticket);
            assert_eq!(ticket.retained_string_payload_charge().bytes, string_bytes);
            let first = RuntimeTermination::new(TerminationReason::TimedOut);
            let shorter = RuntimeTermination {
                reason: TerminationReason::Cancelled,
                cleanup_deadline: first.cleanup_deadline - Duration::from_millis(1),
            };
            let observed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                ticket.finish_with(|result| {
                    assert_eq!(
                        result.as_ref().ok().and_then(s11_copy_owner),
                        Some(owner),
                        "the real operation observer borrows the sole actual owned result"
                    );
                    let _cleanup = CleanupGuard::enter(first);
                    drop(CleanupGuard::enter(shorter));
                    let actual = current_cleanup_context().unwrap();
                    assert_eq!(actual.reason, first.reason);
                    assert_eq!(actual.cleanup_deadline, shorter.cleanup_deadline);
                    observer_tx.send((owner, actual)).unwrap();
                    panic!("S11 actual finish observer native unwind");
                })
            }));
            assert!(observed.is_err());
            assert!(
                current_cleanup_context().is_none(),
                "physical operation-local unwind restores cleanup TLS"
            );
            Ok(Value::Null)
        })
        .unwrap();
    let (observed_owner, context) = observer_rx.recv_timeout(S10_WAIT).unwrap();
    assert_eq!(observed_owner, owner);
    assert_eq!(s10_wait_result(&actor).unwrap(), Value::Null);
    s10_retire(actor, RuntimeTermination::new(TerminationReason::Cancelled));
    assert!(s11_slot_owner(&child).is_none());
    s10_retire(child, context);
    let (value, input) = string_input(PAYLOAD, 4096);
    let (healthy, _) = s10_completed(&runtime, SOURCE, vec![value], input, S10Plan::normal(1));
    assert_eq!(healthy.thread, worker);
    s10_close(&runtime);
}

#[test]
fn claimed_async_await_string_local_saved_q2_q3_and_scopes_keep_every_real_owner_fee() {
    for error in [false, true] {
        let child = format!(
            "async fn Child(text: str): str! {{ r: str! = {}(text)\nreturn r.clone() }}\n",
            if error { "err" } else { "ok" }
        );
        let scoped = format!(
            "{child}async fn Entry(text: str): str! {{ r: str! = await Child(text)\n\
            if (true) {{ inner: str! = r.clone()\ninner }}\nreturn r.clone() }}"
        );
        let q2 = format!("{child}async fn Entry(text: str): str! {{ r: str! = await Child(text)\n\
            try {{ return r.clone() }} finally {{ r\nr = r\nreturn r.clone() }}\nreturn r.clone() }}");
        let q3 = format!(
            "{child}async fn Entry(text: str): str! {{ r: str! = await Child(text)\n\
            try {{ return r.clone() }} finally {{ try {{ return r.clone() }} finally {{ r\nr = r\n\
            return r.clone() }} }}\nreturn r.clone() }}"
        );
        let runtime = TaskRuntime::with_limits(1, 16, 0, 0, 16);
        for (source, saved) in [(&scoped, 0), (&q2, 1), (&q3, 2)] {
            let (value, input) = string_input(PAYLOAD, 4096);
            let (proof, result, ready, bound) =
                s11_completed(&runtime, source, vec![value], input, s11_plan(1, false));
            assert_eq!(ready.len(), 1);
            assert_eq!(bound, 1);
            assert_eq!(proof.max_saved, saved);
            assert_eq!(proof.finally, saved);
            assert_eq!(proof.retired_finally, saved);
            assert_eq!(proof.peak_boxes, proof.observed_peak_boxes);
            assert_eq!(proof.peak_clones, proof.observed_peak_clones);
            assert!(proof.distinct_clones > saved);
            assert_eq!(s9_owner(&result, PAYLOAD).unwrap().error.is_some(), error);
        }
        s10_close(&runtime);
    }
}

struct S11ChildWitness(crate::runtime::task::TaskRetirementWitness);

impl std::fmt::Debug for S11ChildWitness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("actual state-only child retirement witness")
    }
}

fn s11_retire_children(
    children: Vec<crate::runtime::task::TaskRetirementWitness>,
    context: RuntimeTermination,
) {
    use crate::runtime::task::{RetirementBatchPoll, TaskRetirementBatch};
    let mut batch = TaskRetirementBatch::from_reserved(children, context);
    let deadline = Instant::now() + S10_WAIT;
    loop {
        match batch.poll(Instant::now()) {
            RetirementBatchPoll::Retired => break,
            RetirementBatchPoll::TimedOut(actual) => {
                panic!("actual child G1/G2 exceeded D: {actual:?}")
            }
            RetirementBatchPoll::Pending { .. } => {
                assert!(Instant::now() < deadline, "finite real child retirement");
                std::thread::yield_now();
            }
        }
    }
}

fn s11_poll_ready(ticket: &mut crate::runtime::task::AwaitTicket) {
    use crate::runtime::task::AwaitPoll;
    let deadline = Instant::now() + S10_WAIT;
    while ticket.poll() == AwaitPoll::Pending {
        assert!(
            Instant::now() < deadline,
            "finite actual ordinary ticket poll"
        );
        std::thread::yield_now();
    }
}

#[test]
fn claimed_async_await_string_local_waiting_ready_running_produced_cancel_keep_first_cause_and_g1_g2(
) {
    const SOURCE: &str =
        "async fn Child(text: str): str! { r: str! = ok(text)\nreturn r.clone() }\n\
        async fn Entry(text: str): str! { r: str! = await Child(text)\nr\nreturn r.clone() }";
    for stage in [
        S10Pause::AwaitWaiting,
        S10Pause::AwaitReady,
        S10Pause::Running,
        S10Pause::Produced,
    ] {
        let runtime = TaskRuntime::with_limits(1, 16, 0, 0, 16);
        let (value, input) = string_input(PAYLOAD, 4096);
        let mut plan = s11_plan(1, false);
        plan.pause = stage;
        let (task, rx, release) = s10_spawn(&runtime, SOURCE, vec![value], vec![input], plan);
        let worker = s10_entered(&task, &rx, true);
        let mut children = Vec::new();
        let mut proof = None;
        let mut ready = None;
        let paused_owner = loop {
            match rx.recv_timeout(S10_WAIT).unwrap() {
                S10Witness::AwaitChild(witness) => children.push(witness.0),
                S10Witness::AwaitWaiting
                | S10Witness::Progress { .. }
                | S10Witness::AwaitBound(_) => {}
                S10Witness::AwaitReady(actual) => ready = Some(actual.owner),
                S10Witness::Finished(actual) => proof = Some(*actual),
                S10Witness::Paused {
                    stage: actual,
                    owner,
                } => {
                    assert_eq!(actual, stage);
                    break owner.as_deref().copied();
                }
                other => panic!("unexpected actual cancel checkpoint: {other:?}"),
            }
        };
        assert_eq!(children.len(), 1);
        if stage == S10Pause::AwaitReady {
            assert_eq!(paused_owner, ready);
            assert!(
                paused_owner.is_some(),
                "pause retains the real sole ticket owner"
            );
        } else if stage == S10Pause::Produced {
            assert_eq!(paused_owner, proof.as_ref().unwrap().outgoing);
            assert!(
                paused_owner.is_some(),
                "real Break payload exists before publication"
            );
        } else {
            assert!(paused_owner.is_none());
        }
        let first = RuntimeTermination::new(TerminationReason::TimedOut);
        let shorter = RuntimeTermination {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: first.cleanup_deadline - Duration::from_millis(1),
        };
        assert!(task.request_cancel_with(first));
        assert!(!task.request_cancel_with(shorter));
        let expected = RuntimeTermination {
            reason: first.reason,
            cleanup_deadline: shorter.cleanup_deadline,
        };
        release.send(()).unwrap();
        if proof.is_none() {
            loop {
                match rx.recv_timeout(S10_WAIT).unwrap() {
                    S10Witness::Finished(actual) => {
                        proof = Some(*actual);
                        break;
                    }
                    S10Witness::AwaitReady(_)
                    | S10Witness::AwaitBound(_)
                    | S10Witness::AwaitWaiting
                    | S10Witness::Progress { .. } => {}
                    other => panic!("unexpected cancelled actual claimant event: {other:?}"),
                }
            }
        }
        let proof = proof.unwrap();
        assert_eq!(proof.thread, worker);
        assert_eq!(proof.calls, proof.retired);
        assert_eq!(proof.finally, proof.retired_finally);
        if stage != S10Pause::Produced {
            assert!(proof.outgoing.is_none());
        }
        assert_eq!(
            s10_wait_result(&task).unwrap_err().runtime_termination(),
            Some(expected)
        );
        s10_retire(task, expected);
        s11_retire_children(children, expected);
        let (value, input) = string_input(PAYLOAD, 4096);
        let (healthy, _, _, _) =
            s11_completed(&runtime, SOURCE, vec![value], input, s11_plan(1, true));
        assert_eq!(healthy.thread, worker);
        s10_close(&runtime);
    }
}

#[test]
fn claimed_async_await_string_local_real_ready_native_abandonment_retires_then_reuses_worker() {
    const SOURCE: &str =
        "async fn Child(text: str): str! { r: str! = ok(text)\nreturn r.clone() }\n\
        async fn Entry(text: str): str! { r: str! = await Child(text)\nreturn r.clone() }";
    let runtime = TaskRuntime::with_limits(1, 16, 0, 0, 16);
    let (value, input) = string_input(PAYLOAD, 4096);
    let mut plan = s11_plan(1, false);
    plan.fault = S10Fault::NativeReady;
    let (task, rx, _) = s10_spawn(&runtime, SOURCE, vec![value], vec![input], plan);
    let worker = s10_entered(&task, &rx, true);
    let mut children = Vec::new();
    let mut ready = None;
    loop {
        match rx.recv_timeout(S10_WAIT).unwrap() {
            S10Witness::AwaitChild(witness) => children.push(witness.0),
            S10Witness::AwaitReady(actual) => ready = Some(actual.owner),
            S10Witness::AwaitWaiting | S10Witness::Progress { .. } => {}
            S10Witness::Paused {
                stage: S10Pause::AwaitReady,
                owner,
            } => {
                assert_eq!(owner.as_deref().copied(), ready);
                assert!(
                    ready.is_some(),
                    "native unwind starts with the actual Ready payload"
                );
                break;
            }
            other => panic!("unexpected actual Ready native event: {other:?}"),
        }
    }
    let Value::Result { ok: false, value } = s10_wait_result(&task).unwrap() else {
        panic!("existing Task driver native classification");
    };
    let Value::Object(fields) = *value else {
        panic!("actual Task panic ErrorObject");
    };
    assert!(matches!(fields.get("code"), Some(Value::String(code)) if code == "panic"));
    let context = RuntimeTermination::new(TerminationReason::Cancelled);
    s10_retire(task, context);
    s11_retire_children(children, context);
    let (value, input) = string_input(PAYLOAD, 4096);
    let (healthy, _, _, _) = s11_completed(&runtime, SOURCE, vec![value], input, s11_plan(1, true));
    assert_eq!(healthy.thread, worker);
    s10_close(&runtime);
}

fn s11_check_await_quota(machine: &mut Machine, count: usize, bytes: usize) {
    let saved = machine.value_limits;
    machine.value_limits.max_frames = count;
    machine.value_limits.max_storage_bytes = bytes;
    let exact = machine.await_payload_policy(Span::default()).unwrap();
    assert_eq!(
        (exact.max_count, exact.max_bytes),
        (0, 0),
        "compound current owner totals leave no fictitious unused await quota"
    );
    machine.value_limits.max_frames = count - 1;
    assert!(machine
        .await_payload_policy(Span::default())
        .err()
        .unwrap()
        .message
        .contains("FrameLimit"));
    machine.value_limits.max_frames = count;
    machine.value_limits.max_storage_bytes = bytes - 1;
    assert!(machine
        .await_payload_policy(Span::default())
        .err()
        .unwrap()
        .message
        .contains("StorageLimit"));
    machine.value_limits = saved;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum S11HandoffFault {
    None,
    NativeObserver,
    EmitCount,
    EmitBytes,
}

struct S11HandoffState {
    input: Buffer,
    proof: S9Proof,
    fault: S11HandoffFault,
    receipts: Vec<S11HandoffReceipt>,
    saved_limits: Option<FrameLimits>,
    native_context: Option<RuntimeTermination>,
}

#[derive(Debug)]
struct S11HandoffReceipt {
    observer: bool,
    owner: S9Owner,
    shape: AwaitPayloadCharge,
    string_bytes: usize,
    count: usize,
    bytes: usize,
}

struct S11HandoffReport {
    task: i64,
    worker: std::thread::ThreadId,
    receipts: Vec<S11HandoffReceipt>,
    native_context: Option<RuntimeTermination>,
    outgoing: Option<S9Owner>,
    children: Vec<crate::runtime::task::TaskRetirementWitness>,
}

#[test]
fn claimed_async_await_string_local_actual_observer_emit_escrow_quotas_native_and_drop_phases() {
    // This operation probe uses the actual production Machine and Interpreter
    // arbitration on the actual Task worker. The normal runner / from_code
    // path is independently covered above; it is not replaced by this probe.
    for error in [false, true] {
        for fault in [
            S11HandoffFault::None,
            S11HandoffFault::NativeObserver,
            S11HandoffFault::EmitCount,
            S11HandoffFault::EmitBytes,
        ] {
            let source = format!("async fn Child(text: str): str! {{ r: str! = {}(text)\nreturn r.clone() }}\n\
                async fn Entry(text: str): str! {{ r: str! = await Child(text)\nreturn r.clone() }}",
                if error { "err" } else { "ok" });
            let runtime = TaskRuntime::with_limits(1, 16, 0, 0, 16);
            let mut producer = production_interpreter(&source);
            producer.task_runtime = Some(runtime.clone());
            let template = producer.template();
            let function = producer.functions["Entry"].clone();
            let child_runtime = runtime.clone();
            let (value, input) = string_input(PAYLOAD, 4096);
            let (tx, rx) = mpsc::sync_channel(1);
            let actor = runtime.spawn(move || {
                assert!(current_cleanup_context().is_none());
                assert!(crate::runtime::task::current_execution_termination().is_none());
                let task = crate::runtime::task::current_task_id();
                let worker = std::thread::current().id();
                let mut host = Interpreter::from_template(template, child_runtime);
                host.async_execution = true;
                let mut claimant = ClaimedNamedTask::new(host, function, vec![value], Span::default());
                assert_eq!(claimant.task_id, task);
                assert!(claimant.legacy.is_none());
                assert_eq!(claimant.interpreter.steps, 0);
                let mut machine = claimant.machine.take().expect("real selected claimant machine");
                let mut hooks = S9StringFaultProbe::new(&mut claimant.interpreter, Vec::new(), None);
                let mut proof = S9Proof::new(input);
                proof.allow_await_ticket = true;
                hooks.handoff = Some(S11HandoffState {
                    input, proof, fault,
                    receipts: Vec::new(), saved_limits: None, native_context: None,
                });
                let mut children = Vec::new();
                let mut outgoing = None;
                let mut dispatches = 0;
                let result = loop {
                    dispatches += 1;
                    assert!(dispatches <= 100_000, "finite real operation probe work");
                    let state = hooks.handoff.as_mut().unwrap();
                    s9_observe(&machine, input, &mut state.proof);
                    if children.is_empty() && matches!(machine.work.as_slice().last(),
                        Some(Work::AwaitLocalOwnedStringOperand(_)))
                    {
                        if let Some(Value::Task(child)) = machine.values.as_slice().last() {
                            children.push(child.retirement_witness());
                        }
                    }
                    let step = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        machine.dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut hooks)
                    }));
                    if let Some(saved) = hooks.handoff.as_mut().unwrap().saved_limits.take() {
                        machine.value_limits = saved;
                    }
                    match step {
                        Err(_) => {
                            assert_eq!(fault, S11HandoffFault::NativeObserver);
                            assert_eq!(machine.state, MachineState::Aborted);
                            assert_eq!(machine.await_payload_escrow, AwaitPayloadCharge::default());
                            assert_eq!(machine.await_string_payload_escrow.bytes, 0,
                                "outer catch clears only after real operation-local payload unwind");
                            assert!(machine.awaiting.is_none());
                            let context = hooks.handoff.as_ref().unwrap().native_context.unwrap();
                            assert_eq!(crate::runtime::task::current_execution_termination(), Some(context));
                            break Err(KuError::termination(context, Span::default()));
                        }
                        Ok(Err(error)) => break Err(error),
                        Ok(Ok(ChunkProgress::More { popped })) => {
                            assert_eq!(popped, 1);
                            assert_eq!(machine.await_payload_escrow, AwaitPayloadCharge::default());
                            assert_eq!(machine.await_string_payload_escrow.bytes, 0);
                        }
                        Ok(Ok(ChunkProgress::Waiting { popped })) => {
                            assert_eq!(popped, 1);
                            machine.wait_synchronously(hooks.host());
                        }
                        Ok(Ok(ChunkProgress::Finished { popped, result })) => {
                            assert!(popped <= 1);
                            let result = result.and_then(|flow| flow.into_call_value(Span::default(), "function"));
                            outgoing = result.as_ref().ok().and_then(s11_copy_owner);
                            s9_drained(&machine);
                            assert_eq!(machine.await_string_payload_snapshot.bytes, 0);
                            assert_eq!(machine.await_string_payload_escrow.bytes, 0);
                            assert_eq!(machine.metrics.entered_calls, machine.metrics.retired_calls);
                            break result;
                        }
                    }
                };
                let state = hooks.handoff.take().unwrap();
                assert_eq!(children.len(), 1);
                drop(hooks);
                // Every native payload and its ticket has already unwound. The
                // remaining actual Machine prefix drops under its first cause
                // and the same shortened D, without reading freed metrics.
                if let Some(context) = state.native_context {
                    let release = CleanupGuard::enter(context);
                    drop(machine);
                    drop(release);
                } else {
                    drop(machine);
                }
                assert!(current_cleanup_context().is_none());
                tx.send(S11HandoffReport {
                    task, worker, receipts: state.receipts, native_context: state.native_context,
                    outgoing, children,
                }).unwrap();
                result
            }).unwrap();
            let report = rx.recv_timeout(S10_WAIT).unwrap();
            assert_eq!(report.task, actor.id());
            assert_eq!(
                report.receipts.len(),
                if fault == S11HandoffFault::NativeObserver {
                    1
                } else {
                    2
                }
            );
            assert!(report.receipts[0].observer);
            if report.receipts.len() == 2 {
                assert!(!report.receipts[1].observer);
                assert_eq!(
                    report.receipts[0].owner, report.receipts[1].owner,
                    "actual Observer -> target release -> Emit keeps one unchanged owned outcome"
                );
            }
            for phase in &report.receipts {
                assert_eq!(phase.shape, s11_box_error_charge(phase.owner));
                assert_eq!(
                    phase.string_bytes,
                    phase.owner.string.map_or(0, |text| text.capacity)
                );
                assert!(phase.count >= phase.shape.count);
                assert!(phase.bytes >= input.capacity + phase.shape.bytes + phase.string_bytes);
            }
            let result = s10_wait_result(&actor);
            match fault {
                S11HandoffFault::None => {
                    let value = result.unwrap();
                    assert_eq!(s11_copy_owner(&value), report.outgoing);
                    assert_eq!(s9_owner(&value, PAYLOAD).unwrap().error.is_some(), error);
                }
                S11HandoffFault::NativeObserver => {
                    assert_eq!(
                        result.unwrap_err().runtime_termination(),
                        report.native_context
                    );
                }
                S11HandoffFault::EmitCount => {
                    assert!(result.unwrap_err().message.contains("FrameLimit"))
                }
                S11HandoffFault::EmitBytes => {
                    assert!(result.unwrap_err().message.contains("StorageLimit"))
                }
            }
            let context = report
                .native_context
                .unwrap_or_else(|| RuntimeTermination::new(TerminationReason::Cancelled));
            s10_retire(actor, context);
            s11_retire_children(report.children, context);
            let (value, input) = string_input(PAYLOAD, 4096);
            let (healthy, _, _, _) =
                s11_completed(&runtime, &source, vec![value], input, s11_plan(1, true));
            assert_eq!(healthy.thread, report.worker);
            s10_close(&runtime);
        }
    }
}

fn s11_measured_owner(value: &Value, text: &'static str, task_errors: bool) -> Option<S9Owner> {
    if task_errors && matches!(value, Value::Result { ok: false, .. }) {
        // Only S11 task-depth fixtures opt in. They assert the actual runtime
        // code/message after ordinary consuming publication; old source err
        // shape/text assertions continue to run through the unchanged helper.
        s11_copy_owner(value)
    } else {
        s9_owner(value, text)
    }
}

fn s11_completed(
    runtime: &TaskRuntime,
    source: &str,
    args: Vec<Value>,
    input: Buffer,
    plan: S10Plan,
) -> (S10Finished, Value, Vec<S11Ready>, usize) {
    let (proof, result, ready, bound) = s11_receipt(runtime, source, args, input, plan);
    (proof, result.unwrap(), ready, bound)
}

#[test]
fn claimed_async_await_string_local_real_self_mutual_fresh_64_first_65_and_worker_reuse() {
    assert_eq!(crate::runtime::task::MAX_AWAIT_DEPTH, 64);
    for error in [false, true] {
        let leaf = if error { "err(text)" } else { "ok(text)" };
        let branch = |next: &str| {
            format!(
                "if (n <= 0) {{ return {leaf} }}\n\
            r: str! = await {next}(text, n - 1)\nreturn r.clone()"
            )
        };
        let own = format!(
            "async fn Entry(text: str, n: int): str! {{ {} }}",
            branch("Entry")
        );
        let mutual = format!(
            "async fn Other(text: str, n: int): str! {{ {} }}\n\
            async fn Entry(text: str, n: int): str! {{ {} }}",
            branch("Entry"),
            branch("Other")
        );
        let fresh = format!(
            "async fn Child(text: str): str! {{ r: str! = {leaf}\nreturn r.clone() }}\n\
            async fn Entry(text: str, n: int): str! {{ if (n <= 0) {{ r: str! = await Child(text)\n\
            return r.clone() }}\nr: str! = await Entry(text, n - 1)\nreturn r.clone() }}"
        );
        for (source, extra, chunk) in [(&own, 0, 1), (&mutual, 0, 7), (&fresh, 1, 256)] {
            let runtime = TaskRuntime::with_limits(1, 128, 0, 0, 128);
            let mut worker = None;
            for n in [64 - extra, 65 - extra, 1] {
                let (value, input) = string_input(PAYLOAD, 4096);
                let mut plan = s11_plan(chunk, n == 1);
                plan.task_errors = true;
                let (proof, result, ready, bound) =
                    s11_receipt(&runtime, source, vec![value, Value::Int(n)], input, plan);
                if let Some(old) = worker {
                    assert_eq!(old, proof.thread);
                }
                worker = Some(proof.thread);
                assert_eq!(
                    (proof.calls, proof.peak_calls),
                    (1, 1),
                    "logical source calls and actual Task await depth remain distinct"
                );
                assert_eq!(ready.len(), 1);
                assert_eq!(bound, usize::from(chunk == 1));
                let result = result.unwrap();
                if n == 65 - extra {
                    let Value::Result { ok: false, value } = result else {
                        panic!("first actual 65th await preserves the existing controlled error");
                    };
                    let Value::Object(fields) = *value else {
                        panic!("real runtime depth ErrorObject");
                    };
                    assert!(
                        matches!(fields.get("code"), Some(Value::String(code)) if code == "await_depth")
                    );
                    assert!(matches!(fields.get("message"), Some(Value::String(message))
                        if message.contains("64")));
                } else {
                    assert_eq!(s9_owner(&result, PAYLOAD).unwrap().error.is_some(), error);
                }
            }
            s10_close(&runtime);
        }
    }
}

#[test]
fn claimed_async_await_string_local_child_sync_512_first_513_preserves_controlled_error_and_reuse()
{
    for error in [false, true] {
        let leaf = if error { "err(text)" } else { "ok(text)" };
        let source = format!("fn Relay(text: str, n: int): str! {{ if (n <= 0) {{ return {leaf} }}\n\
            return Relay(text, n - 1) }}\n\
            async fn Child(text: str, n: int): str! {{ return Relay(text, n) }}\n\
            async fn Entry(text: str, n: int): str! {{ r: str! = await Child(text, n)\nreturn r.clone() }}");
        let runtime = TaskRuntime::with_limits(1, 16, 0, 0, 16);
        let mut worker = None;
        for n in [510, 511, 0] {
            let (value, input) = string_input(PAYLOAD, 4096);
            let (proof, result, ready, bound) = s11_receipt(
                &runtime,
                &source,
                vec![value, Value::Int(n)],
                input,
                s11_plan(1, n == 0),
            );
            if let Some(old) = worker {
                assert_eq!(old, proof.thread);
            }
            worker = Some(proof.thread);
            assert_eq!((proof.calls, proof.retired), (1, 1));
            if n == 511 {
                assert_eq!(
                    result.unwrap_err().code.as_deref(),
                    Some("call_depth_exceeded")
                );
                assert!(
                    ready.is_empty(),
                    "raw child failure transfers no invented Result payload"
                );
                assert_eq!(bound, 0);
                assert!(proof.outgoing.is_none());
            } else {
                assert_eq!(ready.len(), 1);
                assert_eq!(bound, 1);
                assert_eq!(
                    s9_owner(&result.unwrap(), PAYLOAD).unwrap().error.is_some(),
                    error
                );
            }
        }
        s10_close(&runtime);
    }
}

#[test]
fn claimed_async_await_string_local_queued_factory_cancel_and_last_owner_release_retire_real_prefixes(
) {
    const SOURCE: &str =
        "async fn Child(text: str): str! { r: str! = ok(text)\nreturn r.clone() }\n\
        async fn Entry(text: str): str! { r: str! = await Child(text)\nr\nreturn r.clone() }";
    let runtime = TaskRuntime::with_limits(1, 16, 0, 0, 16);
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (held_tx, held_rx) = mpsc::sync_channel(1);
    let held = runtime
        .spawn(move || {
            ready_tx.send(()).unwrap();
            held_rx.recv_timeout(S10_WAIT).unwrap();
            Ok(Value::Null)
        })
        .unwrap();
    ready_rx.recv_timeout(S10_WAIT).unwrap();
    let (value, input) = string_input(PAYLOAD, 4096);
    let (queued, rx, _) = s10_spawn(
        &runtime,
        SOURCE,
        vec![value],
        vec![input],
        s11_plan(1, true),
    );
    assert_eq!(runtime.snapshot().unwrap().queued_tasks, 1);
    let first = RuntimeTermination::new(TerminationReason::TimedOut);
    assert!(queued.request_cancel_with(first));
    assert!(matches!(rx.recv_timeout(S10_WAIT), Err(mpsc::RecvTimeoutError::Disconnected)),
        "the actual deferred S11 factory and original argument drop before any constructor dispatch");
    // The queued cancellation has already published its terminal outcome.
    // A later request cannot rewrite that published first cause or deadline.
    assert!(!queued.request_cancel_with(RuntimeTermination {
        reason: TerminationReason::Cancelled,
        cleanup_deadline: first.cleanup_deadline - Duration::from_millis(1),
    }));
    assert_eq!(
        s10_wait_result(&queued).unwrap_err().runtime_termination(),
        Some(first)
    );
    s10_retire(queued, first);
    held_tx.send(()).unwrap();
    assert_eq!(s10_wait_result(&held).unwrap(), Value::Null);
    s10_retire(held, RuntimeTermination::new(TerminationReason::Cancelled));

    let (value, input) = string_input(PAYLOAD, 4096);
    let mut plan = s11_plan(1, false);
    plan.pause = S10Pause::Produced;
    let (task, rx, release) = s10_spawn(&runtime, SOURCE, vec![value], vec![input], plan);
    let worker = s10_entered(&task, &rx, true);
    let mut children = Vec::new();
    let mut proof = None;
    loop {
        match rx.recv_timeout(S10_WAIT).unwrap() {
            S10Witness::AwaitChild(witness) => children.push(witness.0),
            S10Witness::Finished(actual) => proof = Some(*actual),
            S10Witness::AwaitWaiting
            | S10Witness::AwaitReady(_)
            | S10Witness::AwaitBound(_)
            | S10Witness::Progress { .. } => {}
            S10Witness::Paused {
                stage: S10Pause::Produced,
                owner,
            } => {
                assert_eq!(owner.as_deref().copied(), proof.as_ref().unwrap().outgoing);
                assert!(
                    owner.is_some(),
                    "actual unpublished Break owner remains on the real worker"
                );
                break;
            }
            other => panic!("unexpected last-owner actual checkpoint: {other:?}"),
        }
    }
    assert_eq!(children.len(), 1);
    let proof = proof.unwrap();
    assert_eq!(proof.thread, worker);
    let witness = task.retirement_witness();
    let context = RuntimeTermination::new(TerminationReason::Cancelled);
    let window = CleanupGuard::enter(context);
    drop(task);
    drop(window);
    let mut batch =
        crate::runtime::task::TaskRetirementBatch::from_reserved(vec![witness], context);
    assert!(
        matches!(
            batch.poll(Instant::now()),
            crate::runtime::task::RetirementBatchPoll::Pending { .. }
        ),
        "last owner release cannot acknowledge the still-live incoming payload and running node"
    );
    release.send(()).unwrap();
    let deadline = Instant::now() + S10_WAIT;
    loop {
        match batch.poll(Instant::now()) {
            crate::runtime::task::RetirementBatchPoll::Retired => break,
            crate::runtime::task::RetirementBatchPoll::TimedOut(actual) => {
                panic!("actual last owner D expired: {actual:?}")
            }
            crate::runtime::task::RetirementBatchPoll::Pending { .. } => {
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
        }
    }
    assert_eq!(batch.context(), context);
    s11_retire_children(children, context);
    let (value, input) = string_input(PAYLOAD, 4096);
    let (healthy, _, _, _) = s11_completed(&runtime, SOURCE, vec![value], input, s11_plan(1, true));
    assert_eq!(healthy.thread, worker);
    s10_close(&runtime);
}

#[test]
fn claimed_async_await_string_local_post_await_protected_finally_cancellation_keeps_actual_owners_and_d(
) {
    for error in [false, true] {
        let source = format!(
            "async fn Child(text: str): str! {{ r: str! = {}(text)\nreturn r.clone() }}\n\
            async fn Entry(text: str): str! {{ r: str! = await Child(text)\n\
            try {{ marker = 1\nmarker = 2 }} finally {{ r\nr = r }}\nreturn r.clone() }}",
            if error { "err" } else { "ok" }
        );
        let declared = functions(&source);
        let Stmt::Try { body, .. } = &declared["Entry"].body[1] else {
            panic!("actual post-await protected source");
        };
        let target = stmt_span(&body[1]);
        let runtime = TaskRuntime::with_limits(1, 16, 0, 0, 16);
        let (value, input) = string_input(PAYLOAD, 4096);
        let mut plan = s11_plan(1, false);
        plan.pause = S10Pause::Protected(target);
        let (task, rx, release) = s10_spawn(&runtime, &source, vec![value], vec![input], plan);
        let worker = s10_entered(&task, &rx, true);
        let mut children = Vec::new();
        let mut ready = 0;
        let mut bound = 0;
        loop {
            match rx.recv_timeout(S10_WAIT).unwrap() {
                S10Witness::AwaitChild(witness) => children.push(witness.0),
                S10Witness::AwaitReady(_) => ready += 1,
                S10Witness::AwaitBound(_) => bound += 1,
                S10Witness::AwaitWaiting | S10Witness::Progress { .. } => {}
                S10Witness::Paused {
                    stage: S10Pause::Protected(actual),
                    owner: None,
                } => {
                    assert_eq!(actual, target);
                    break;
                }
                other => panic!("unexpected real protected post-await checkpoint: {other:?}"),
            }
        }
        assert_eq!((children.len(), ready, bound), (1, 1, 1));
        let first = RuntimeTermination::new(TerminationReason::Cancelled);
        let shorter = RuntimeTermination {
            reason: TerminationReason::TimedOut,
            cleanup_deadline: first.cleanup_deadline - Duration::from_millis(1),
        };
        assert!(task.request_cancel_with(first));
        assert!(!task.request_cancel_with(shorter));
        let expected = RuntimeTermination {
            reason: first.reason,
            cleanup_deadline: shorter.cleanup_deadline,
        };
        release.send(()).unwrap();
        let proof = loop {
            match rx.recv_timeout(S10_WAIT).unwrap() {
                S10Witness::Finished(actual) => break *actual,
                S10Witness::Progress { .. } => {}
                other => panic!("unexpected real protected cleanup event: {other:?}"),
            }
        };
        assert_eq!(
            s10_wait_result(&task).unwrap_err().runtime_termination(),
            Some(expected)
        );
        assert_eq!(proof.thread, worker);
        assert_eq!((proof.finally, proof.retired_finally), (1, 1));
        assert!(
            proof.distinct_clones >= 2,
            "actual protected r read/assignment ran after the completed child await"
        );
        assert_eq!(proof.peak_boxes, proof.observed_peak_boxes);
        assert_eq!(proof.peak_clones, proof.observed_peak_clones);
        assert!(proof.outgoing.is_none());
        s10_retire(task, expected);
        s11_retire_children(children, expected);
        let (value, input) = string_input(PAYLOAD, 4096);
        let (healthy, _, _, _) =
            s11_completed(&runtime, &source, vec![value], input, s11_plan(1, true));
        assert_eq!(healthy.thread, worker);
        s10_close(&runtime);
    }
}

#[test]
fn claimed_async_await_string_local_new_policy_private_sparse_error_slot_charges_actual_capacity() {
    use crate::runtime::task::{AwaitStringPayloadPolicy, AwaitTicket};
    // This is deliberately a private real-slot capacity fixture, as in the
    // existing await_string sparse Map test. It claims no source-level Object
    // construction or String cloning capability in the admitted language slice.
    let runtime = TaskRuntime::with_limits(1, 16, 0, 0, 16);
    for reason in [Some("FrameLimit"), Some("StorageLimit"), None] {
        let mut fields = HashMap::with_capacity(128);
        for (name, text, capacity) in [
            ("domain", "task", 128),
            ("code", "sparse", 512),
            ("message", "retained sparse error", 4096),
        ] {
            let mut key = String::with_capacity(64);
            key.push_str(name);
            let mut value = String::with_capacity(capacity);
            value.push_str(text);
            fields.insert(key, Value::String(value));
        }
        let child = runtime
            .spawn(move || {
                Ok(Value::Result {
                    ok: false,
                    value: Box::new(Value::Object(fields)),
                })
            })
            .unwrap();
        s11_wait_published(&child);
        let actual = s11_slot_owner(&child).unwrap();
        let error = actual.error.unwrap();
        assert!(error.count > 3);
        let measured = s11_box_error_charge(actual);
        assert_eq!(measured.error_leaf_count, error.count);
        assert_eq!(measured.error_leaf_bytes, error.bytes);
        let mut ticket = AwaitTicket::begin_with_string_payload_policy(
            child.clone(),
            AwaitStringPayloadPolicy::new(AwaitPayloadPolicy {
                max_count: measured.count - usize::from(reason == Some("FrameLimit")),
                max_bytes: measured.bytes - usize::from(reason == Some("StorageLimit")),
                input_string_capacity: 0,
                span: Span::default(),
            }),
        );
        s11_poll_ready(&mut ticket);
        assert_eq!(ticket.retained_string_payload_charge().bytes, 0,
            "six actual error key/value buffers belong to error charge, never success String charge");
        if let Some(reason) = reason {
            assert_eq!(s11_slot_owner(&child), Some(actual));
            assert_eq!(
                ticket.retained_payload_charge(),
                AwaitPayloadCharge::default()
            );
            assert!(ticket
                .finish_with(|_| {})
                .unwrap_err()
                .message
                .contains(reason));
            s11_rejected_ordinary_slot_keeps_claim_and_payload(&child, actual);
        } else {
            assert!(s11_slot_owner(&child).is_none());
            assert_eq!(ticket.retained_payload_charge(), measured);
            let mut observer = None;
            let returned = ticket
                .finish_with(|result| {
                    observer = result.as_ref().ok().and_then(s11_copy_owner);
                })
                .unwrap();
            assert_eq!(observer, Some(actual));
            assert_eq!(s11_copy_owner(&returned), Some(actual));
        }
        s10_retire(child, RuntimeTermination::new(TerminationReason::Cancelled));
    }
    s10_close(&runtime);
}

struct S11NativeTargetCapture {
    observer_ran: Arc<std::sync::atomic::AtomicBool>,
    drops: Arc<std::sync::atomic::AtomicUsize>,
    shorter: RuntimeTermination,
    tx: mpsc::SyncSender<(
        bool,
        Option<RuntimeTermination>,
        Option<RuntimeTermination>,
        std::thread::ThreadId,
    )>,
}

impl Drop for S11NativeTargetCapture {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        let before = current_cleanup_context();
        drop(CleanupGuard::enter(self.shorter));
        let after = current_cleanup_context();
        assert_eq!(self.drops.fetch_add(1, Ordering::AcqRel), 0);
        self.tx
            .send((
                self.observer_ran.load(Ordering::Acquire),
                before,
                after,
                std::thread::current().id(),
            ))
            .unwrap();
        panic!("S11 actual owned target release capture destructor panic");
    }
}

#[test]
fn claimed_async_await_string_local_new_policy_actual_target_release_native_drop_preserves_first_d()
{
    use crate::runtime::task::{AwaitStringPayloadPolicy, AwaitTicket};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let runtime = TaskRuntime::with_limits(1, 16, 0, 0, 16);
    let child_runtime = runtime.clone();
    let (report_tx, report_rx) = mpsc::sync_channel(1);
    let actor = runtime
        .spawn(move || {
            assert!(current_cleanup_context().is_none());
            assert!(crate::runtime::task::current_execution_termination().is_none());
            let task = crate::runtime::task::current_task_id();
            let worker = std::thread::current().id();
            let observer_ran = Arc::new(AtomicBool::new(false));
            let drops = Arc::new(AtomicUsize::new(0));
            let child_ran = Arc::new(AtomicBool::new(false));
            let first = RuntimeTermination::new(TerminationReason::TimedOut);
            let observer_shorter = RuntimeTermination {
                reason: TerminationReason::Cancelled,
                cleanup_deadline: first.cleanup_deadline - Duration::from_millis(1),
            };
            let target_shorter = RuntimeTermination {
                reason: TerminationReason::Cancelled,
                cleanup_deadline: observer_shorter.cleanup_deadline - Duration::from_millis(1),
            };
            let (drop_tx, drop_rx) = mpsc::sync_channel(1);
            let capture = S11NativeTargetCapture {
                observer_ran: Arc::clone(&observer_ran),
                drops: Arc::clone(&drops),
                shorter: target_shorter,
                tx: drop_tx,
            };
            let ran = Arc::clone(&child_ran);
            // This actual actor occupies the only worker. Its actual child keeps
            // the native captured owner queued until unique target release below.
            let child = child_runtime
                .spawn(move || {
                    ran.store(true, Ordering::Release);
                    drop(capture);
                    Ok(Value::Null)
                })
                .unwrap();
            let witness = child.retirement_witness();
            let ambient = CleanupGuard::enter(first);
            let mut ticket = AwaitTicket::begin_with_string_payload_policy(
                child,
                AwaitStringPayloadPolicy::new(AwaitPayloadPolicy {
                    max_count: usize::MAX,
                    max_bytes: usize::MAX,
                    input_string_capacity: 0,
                    span: Span::default(),
                }),
            );
            // Admission rejects the actual operation before taking its queued
            // target. The Ready outcome is termination with no String owner; this
            // native target test therefore makes no nonzero-String escrow claim.
            s11_poll_ready(&mut ticket);
            assert_eq!(drops.load(Ordering::Acquire), 0);
            assert_eq!(
                ticket.retained_payload_charge(),
                AwaitPayloadCharge::default()
            );
            assert_eq!(ticket.retained_string_payload_charge().bytes, 0);
            drop(ambient);
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                ticket.finish_with(|result| {
                    assert_eq!(
                        result.as_ref().err().unwrap().runtime_termination(),
                        Some(first)
                    );
                    observer_ran.store(true, Ordering::Release);
                    drop(CleanupGuard::enter(observer_shorter));
                })
            }))
            .expect_err("the actual target destructor, after the normal observer, must panic");
            assert_eq!(
                panic.downcast_ref::<&str>().copied(),
                Some("S11 actual owned target release capture destructor panic")
            );
            let (observed, before, after, drop_worker) = drop_rx.recv_timeout(S10_WAIT).unwrap();
            let expected_before = RuntimeTermination {
                reason: first.reason,
                cleanup_deadline: observer_shorter.cleanup_deadline,
            };
            let expected_after = RuntimeTermination {
                reason: first.reason,
                cleanup_deadline: target_shorter.cleanup_deadline,
            };
            assert!(
                observed,
                "normal observer completed before actual unique target owner release"
            );
            assert_eq!(
                drop_worker, worker,
                "native destructor runs on this actual sole claimant worker"
            );
            assert_eq!(before, Some(expected_before));
            assert_eq!(after, Some(expected_after));
            assert_eq!(drops.load(Ordering::Acquire), 1);
            assert!(!child_ran.load(Ordering::Acquire));
            assert!(current_cleanup_context().is_none());
            assert!(crate::runtime::task::current_execution_termination().is_none());
            // Retire its real cancelled queue node while the actor still owns the
            // worker. The state-only receipt neither invokes nor invents a body.
            s11_retire_children(vec![witness], expected_after);
            report_tx.send((task, worker, expected_after)).unwrap();
            Ok(Value::Null)
        })
        .unwrap();
    let (task, worker, _actual_first_context) = report_rx.recv_timeout(S10_WAIT).unwrap();
    assert_eq!(task, actor.id());
    assert_eq!(s10_wait_result(&actor).unwrap(), Value::Null);
    s10_retire(actor, RuntimeTermination::new(TerminationReason::Cancelled));
    let (healthy_tx, healthy_rx) = mpsc::sync_channel(1);
    let healthy = runtime
        .spawn(move || {
            assert!(current_cleanup_context().is_none());
            assert!(crate::runtime::task::current_execution_termination().is_none());
            healthy_tx.send(std::thread::current().id()).unwrap();
            Ok(Value::Null)
        })
        .unwrap();
    assert_eq!(healthy_rx.recv_timeout(S10_WAIT).unwrap(), worker);
    assert_eq!(s10_wait_result(&healthy).unwrap(), Value::Null);
    s10_retire(
        healthy,
        RuntimeTermination::new(TerminationReason::Cancelled),
    );
    s10_close(&runtime);
}

#[test]
fn string_result_parameter_real_worker_recursion_is_bounded_and_reusable() {
    let (_, test_name) = concat!(
        module_path!(),
        "::string_result_parameter_real_worker_recursion_is_bounded_and_reusable"
    )
    .split_once("::")
    .unwrap();
    if !bounded_worker_child(test_name) {
        return;
    }
    const RELAY: &str = "fn Relay(value: str!, n: int): str! { if (n <= 0) { return value }\nreturn Relay(value, n - 1) }";
    for error in [false, true] {
        let constructor = if error { "err" } else { "ok" };
        // Preserve the checked ordinary async entry that originally aborted
        // at 127 calls, including its existing business-error catch path.
        for n in [0, 127] {
            let source = format!("{RELAY}\nasync fn Worker(): str! {{ return Relay({constructor}(\"owned payload\"), {n}) }}\nasync fn main(): null! {{ try {{ print((await Worker())?) }} catch (e) {{ print(e.domain)\nprint(e.code)\nprint(e.message) }}\nreturn ok(null) }}");
            crate::cli::run_source("string-result-parameter-worker.ku", &source).unwrap();
        }
        let source =
            format!("{RELAY}\nfn Seed(): str! {{ return {constructor}(\"owned payload\") }}");
        let runtime = TaskRuntime::with_limits(1, 16, 0, 0, 16);
        let mut previous_worker = None;
        for n in [511, 512, 0] {
            let mut host = production_interpreter(&source);
            let (tx, rx) = mpsc::sync_channel(1);
            let task = runtime
                .spawn(move || {
                    let input = host.call_function("Seed", vec![], Span::default(), 0)?;
                    let result =
                        host.call_function("Relay", vec![input, Value::Int(n)], Span::default(), 0);
                    tx.send((std::thread::current().id(), result, host.call_depth))
                        .unwrap();
                    Ok(Value::Null)
                })
                .unwrap();
            // A channel wait cannot help the job on the caller's larger stack.
            let (worker, result, depth) = rx.recv_timeout(S10_WAIT).unwrap();
            assert_ne!(worker, std::thread::current().id());
            if let Some(previous) = previous_worker {
                assert_eq!(worker, previous);
            }
            previous_worker = Some(worker);
            if n == 512 {
                assert_eq!(
                    result.unwrap_err().code.as_deref(),
                    Some("call_depth_exceeded")
                );
            } else {
                assert_eq!(
                    s9_owner(&result.unwrap(), "owned payload")
                        .unwrap()
                        .error
                        .is_some(),
                    error
                );
            }
            assert_eq!(depth, 0);
            assert_eq!(s10_wait_result(&task).unwrap(), Value::Null);
            s10_retire(task, RuntimeTermination::new(TerminationReason::Cancelled));
        }
        s10_close(&runtime);
    }
    eprintln!("worker regression completed: {test_name}");
}

#[test]
fn string_result_parameter_compound_capacity_rejection_preserves_original_owners() {
    const SOURCE: &str = "fn Entry(value: str!, n: int): str! { return value }";
    let source = functions(SOURCE);
    let code = CodeHandle::compile_owned_string_result_parameter_named(
        &source,
        &source["Entry"],
        Limits::default(),
    )
    .unwrap();
    for error in [false, true] {
        for count_rejection in [false, true] {
            let value = if error {
                let mut fields = HashMap::with_capacity(128);
                for (name, text) in [("domain", "ku"), ("code", "err"), ("message", PAYLOAD)] {
                    let mut key = String::with_capacity(64);
                    key.push_str(name);
                    let (value, _) = string_input(text, 4096);
                    fields.insert(key, value);
                }
                Value::Result {
                    ok: false,
                    value: Box::new(Value::Object(fields)),
                }
            } else {
                let (value, _) = string_input(PAYLOAD, 4096);
                Value::Result {
                    ok: true,
                    value: Box::new(value),
                }
            };
            let original = s9_owner(&value, PAYLOAD).unwrap();
            let mut args = Vec::with_capacity(64);
            args.extend([value, Value::Int(0)]);
            let original_vec = (args.as_ptr(), args.capacity());
            // Independent arithmetic uses actual spare Map/Vec capacity and
            // all six error buffers, rather than the three-entry schema size.
            let box_bytes = std::mem::size_of::<Value>();
            let (owner_count, owner_bytes) = original.error.map_or_else(
                || (1, box_bytes + original.string.unwrap().capacity),
                |actual| (1 + actual.count, box_bytes + actual.bytes),
            );
            let mut limits = Limits::default();
            if count_rejection {
                limits.values.max_frames = args.capacity() + owner_count - 1;
            } else {
                limits.values.max_storage_bytes = args.capacity() * box_bytes + owner_bytes - 1;
            }
            let mut machine = Machine::from_code(code.clone(), limits, 0);
            let mut host = production_interpreter(SOURCE);
            let failure = machine
                .run_owned_with_hooks("Entry", &mut args, 0, false, &mut host)
                .err()
                .expect("compound capacity must reject before transferring input");
            assert!(failure.message.contains(if count_rejection {
                "FrameLimit"
            } else {
                "StorageLimit"
            }));
            assert_eq!((args.as_ptr(), args.capacity()), original_vec);
            assert_eq!(args.len(), 2);
            assert_eq!(s9_owner(&args[0], PAYLOAD), Some(original));
            assert_eq!((host.steps, machine.metrics.entered_calls), (0, 0));
            s9_drained(&machine);
            // The exact retained input and Machine remain usable after the
            // failed admission; the legacy transparent clone need not keep
            // the same Box or String backing in the returned result.
            machine.value_limits = Limits::default().values;
            let result = machine
                .run_owned_with_hooks("Entry", &mut args, 0, false, &mut host)
                .unwrap()
                .into_call_value(Span::default(), "function")
                .unwrap();
            assert_eq!(s9_owner(&result, PAYLOAD).unwrap().error.is_some(), error);
            assert!(args.is_empty() && args.capacity() == 0);
            assert_eq!(host.call_depth, 0);
            s9_drained(&machine);
        }
    }
}
