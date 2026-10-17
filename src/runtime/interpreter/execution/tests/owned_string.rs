use super::*;

const FORWARD: &str = r#"
fn Forward(text: str, n: int): str {
    if (n <= 0) { return text }
    return Forward(text, n - 1)
}
"#;

fn input(n: i64) -> (Vec<Value>, *const u8, usize) {
    let mut text = String::with_capacity(4096);
    text.push_str("kept\0payload");
    let pointer = text.as_ptr();
    let capacity = text.capacity();
    (vec![Value::String(text), Value::Int(n)], pointer, capacity)
}

#[test]
fn owned_string_recursion_moves_the_original_buffer_through_512_real_calls() {
    let source = functions(FORWARD);
    let code =
        CodeHandle::compile_owned_named(&source, &source["Forward"], Limits::default()).unwrap();
    let mut machine = Machine::from_code(code, Limits::default(), 0);
    let (mut args, pointer, capacity) = input(511);
    let mut interpreter = production_interpreter(FORWARD);
    let result = machine
        .run_owned_with_hooks("Forward", &mut args, 0, false, &mut interpreter)
        .unwrap()
        .into_call_value(Span::default(), "function")
        .unwrap();
    let Value::String(text) = result else {
        panic!("original owner must return");
    };
    assert_eq!(text.as_ptr(), pointer);
    assert_eq!(text.capacity(), capacity);
    assert_eq!(text, "kept\0payload");
    assert!(args.is_empty() && args.capacity() == 0);
    assert_eq!(machine.metrics.entered_calls, 512);
    assert_eq!(machine.metrics.peak_calls, 512);
    assert_eq!(machine.metrics.peak_string_payload_bytes, capacity);
    assert_eq!(machine.metrics.retained_string_payload_bytes, 0);
    assert_drained(&machine.metrics, 0);
    let (args, _, _) = input(4);
    let actual = interpreter
        .call_function("Forward", args, Span::default(), 0)
        .unwrap();
    assert_eq!(actual, Value::String("kept\0payload".into()));
    assert_eq!(interpreter.call_depth, 0);
}

#[test]
fn owned_string_depth_and_payload_budget_rejections_never_restart_the_body() {
    let source = functions(FORWARD);
    for oversized in [false, true] {
        let code = CodeHandle::compile_owned_named(&source, &source["Forward"], Limits::default())
            .unwrap();
        let mut limits = Limits::default();
        let (mut args, pointer, capacity) = input(if oversized { 1 } else { 512 });
        if oversized {
            limits.values.max_storage_bytes = capacity - 1;
        }
        let mut machine = Machine::from_code(code, limits, 0);
        let mut interpreter = production_interpreter(FORWARD);
        let error = machine
            .run_owned_with_hooks("Forward", &mut args, 0, false, &mut interpreter)
            .err()
            .expect("real admission/depth boundary must reject");
        if oversized {
            assert!(error
                .message
                .contains("internal execution storage rejected"));
            assert_eq!(machine.metrics.entered_calls, 0);
            assert_eq!(interpreter.steps, 0);
            let Value::String(text) = &args[0] else {
                unreachable!()
            };
            assert_eq!(text.as_ptr(), pointer);
        } else {
            assert_eq!(error.code.as_deref(), Some("call_depth_exceeded"));
            assert_eq!(machine.metrics.entered_calls, 512);
            assert!(args.is_empty());
        }
        assert_eq!((machine.argument_capacity, machine.argument_bytes), (0, 0));
        assert_drained(&machine.metrics, 0);
    }
}

#[test]
fn unsupported_string_operations_leave_the_actual_input_and_legacy_behavior_intact() {
    for source in [
        "fn Entry(text: str): str { return text + \"!\" }",
        "fn Read(&text: str): int { return text.len() }\nfn Entry(text: str): int { return Read(text) }",
    ] {
        let mut interpreter = production_interpreter(source);
        let entry = interpreter.functions.get("Entry").unwrap().clone();
        let (mut args, pointer, _) = input(0);
        args.pop();
        assert!(try_execute_owned(&mut interpreter, &entry, &mut args, 0).is_none());
        assert_eq!(interpreter.steps, 0);
        let Value::String(text) = &args[0] else { unreachable!() };
        assert_eq!(text.as_ptr(), pointer);
        let result = interpreter.call_function("Entry", args, Span::default(), 0).unwrap();
        if source.contains("text +") { assert_eq!(result, Value::String("kept\0payload!".into())); }
        else { assert_eq!(result, Value::Int(12)); }
        assert_eq!(interpreter.call_depth, 0);
    }
}

#[test]
fn owned_string_partial_entry_and_transferred_vec_rejection_retire_the_real_owners() {
    let source = functions("fn Entry(first: str, second: str): str { return first }");
    for reject_before_binding in [false, true] {
        let code =
            CodeHandle::compile_owned_named(&source, &source["Entry"], Limits::default()).unwrap();
        let (mut args, _, _) = input(0);
        let mut limits = Limits::default();
        if reject_before_binding {
            // The actual incoming Vec fits, but its simultaneously retained
            // backing leaves no operand slots. Failure follows transfer.
            limits.values.max_frames = args.capacity();
        }
        let mut machine = Machine::from_code(code, limits, 0);
        let mut interpreter =
            production_interpreter("fn Entry(first: str, second: str): str { return first }");
        // Deliberate internal binding failure: first real String has moved,
        // second declared String sees the original primitive input slot.
        // This is not source admission of an ill-typed call.
        let error = machine
            .run_owned_with_hooks("Entry", &mut args, 0, false, &mut interpreter)
            .err()
            .expect("real partial binding/reservation must fail");
        assert!(args.is_empty() && args.capacity() == 0);
        assert!(machine.entry_args.is_none());
        assert_eq!((machine.argument_capacity, machine.argument_bytes), (0, 0));
        assert_eq!(interpreter.steps, 0, "entry body never started");
        assert_eq!(
            machine.metrics.entered_calls,
            usize::from(!reject_before_binding)
        );
        if reject_before_binding {
            assert!(error
                .message
                .contains("internal execution storage rejected"));
        }
        assert_drained(&machine.metrics, 0);
    }
}

#[test]
fn finally_replaces_the_return_with_the_second_original_string_buffer() {
    let text =
        "fn Entry(first: str, second: str, fallback: str): str { try { return first } finally { return second }\nreturn fallback }";
    let source = functions(text);
    let code =
        CodeHandle::compile_owned_named(&source, &source["Entry"], Limits::default()).unwrap();
    let mut first = String::with_capacity(1024);
    first.push_str("first");
    let mut second = String::with_capacity(2048);
    second.push_str("second");
    let expected_pointer = second.as_ptr();
    let fallback = String::from("unreachable");
    let payload = first.capacity() + second.capacity() + fallback.capacity();
    let mut args = vec![
        Value::String(first),
        Value::String(second),
        Value::String(fallback),
    ];
    let mut machine = Machine::from_code(code, Limits::default(), 0);
    let mut interpreter = production_interpreter(text);
    let Value::String(result) = machine
        .run_owned_with_hooks("Entry", &mut args, 0, false, &mut interpreter)
        .unwrap()
        .into_call_value(Span::default(), "function")
        .unwrap()
    else {
        panic!("finally must transfer the replacement String");
    };
    assert_eq!(result, "second");
    assert_eq!(result.as_ptr(), expected_pointer);
    assert_eq!(machine.metrics.peak_string_payload_bytes, payload);
    assert_eq!(machine.metrics.retained_string_payload_bytes, 0);
    assert_drained(&machine.metrics, 0);
}
