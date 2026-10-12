use super::*;

const TEXT: &str = "\u{4e2d}\u{e9}\0";
const READ: &str = r#"
fn Read(&text: str, n: int): int! {
    if (n <= 0) { return ok(text.len()) }
    return ok(Read(text, n - 1)?)
}
"#;

fn text_input() -> (String, *const u8, usize) {
    let mut text = String::with_capacity(4096);
    text.push_str(TEXT);
    let pointer = text.as_ptr();
    let capacity = text.capacity();
    (text, pointer, capacity)
}

fn machine(source: &str, limits: Limits) -> Machine {
    let source = functions(source);
    let code = CodeHandle::compile_borrowed_string_named(&source, &source["Read"], limits).unwrap();
    Machine::from_code(code, limits, 0)
}

fn root_view() -> (Env, Value, Option<BorrowLease>, *const u8, usize) {
    let (text, pointer, capacity) = text_input();
    let mut owner = Env::new();
    owner
        .define_owned("text".into(), Value::String(text), false, Span::default())
        .unwrap();
    let (view, lease) = owner.borrow("text", Span::default()).unwrap();
    (owner, view, lease, pointer, capacity)
}

fn check_text(value: &Value, pointer: *const u8, capacity: usize) -> KuResult<()> {
    value.with_read(Span::default(), |value| {
        let Value::String(text) = value else {
            panic!("the original String root must stay available")
        };
        assert_eq!((text.as_ptr(), text.capacity()), (pointer, capacity));
        assert_eq!(text, TEXT);
        Ok(())
    })
}

fn check_owner(owner: &Env, pointer: *const u8, capacity: usize, borrowed: bool) {
    owner
        .with_value("text", Span::default(), |value| {
            assert_eq!(matches!(value, Value::Borrowed(_)), borrowed);
            check_text(value, pointer, capacity)
        })
        .unwrap();
}

fn observed_view(machine: &Machine, name: &str) -> crate::value::BorrowedValue {
    machine
        .calls
        .get(0)
        .expect("the real borrowed parameter frame remains live")
        .env
        .with_value(name, Span::default(), |value| {
            let Value::Borrowed(view) = value else {
                panic!("the actual parameter must be a weak read view")
            };
            Ok(view.clone()) // Only weak view metadata; the String is not cloned.
        })
        .unwrap()
}

fn wrapper_count(machine: &Machine) -> usize {
    let is_wrapper = |value: &Value| {
        matches!(value, Value::Result { value, .. }
        if matches!(value.as_ref(), Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Null))
    };
    let in_flow = |result: &KuResult<Flow>| {
        matches!(result,
        Ok(Flow::Return { value, .. } | Flow::Fail { value, .. }) if is_wrapper(value))
    };
    let count = machine
        .values
        .as_slice()
        .iter()
        .filter(|value| is_wrapper(value))
        .count()
        + usize::from(machine.pending.as_ref().is_some_and(in_flow))
        + machine
            .exits
            .as_slice()
            .iter()
            .filter(|exit| exit.state.outcome().is_some_and(in_flow))
            .count()
        + machine
            .finally
            .frames
            .as_slice()
            .iter()
            .filter(|frame| frame.primary.as_ref().is_some_and(in_flow))
            .count();
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

fn top_work(machine: &Machine) -> Option<&Work> {
    machine
        .work
        .len()
        .checked_sub(1)
        .and_then(|index| machine.work.get(index))
}

fn at_wrap(machine: &Machine) -> bool {
    machine.pending.is_none()
        && machine.exits.len() == 0
        && matches!(top_work(machine), Some(Work::WrapBorrowedTerminalOk(_)))
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
            _ => panic!("the requested real borrowed Result checkpoint was not reached"),
        }
    }
    panic!("the finite borrowed Result checkpoint did not finish");
}

fn finish(machine: &mut Machine, host: &mut Interpreter, chunk: usize) -> KuResult<Value> {
    for _ in 0..100_000 {
        match machine
            .dispatch_chunk(NonZeroUsize::new(chunk).unwrap(), host)
            .unwrap()
        {
            ChunkProgress::More { popped } => {
                assert_eq!(popped, chunk);
                wrapper_count(machine);
            }
            ChunkProgress::Waiting { .. } => {
                panic!("this synchronous borrowed Result fixture has no await")
            }
            ChunkProgress::Finished { popped, result } => {
                assert!(popped <= chunk);
                return result.and_then(|flow| flow.into_call_value(Span::default(), "function"));
            }
        }
    }
    panic!("the finite borrowed Result fixture did not finish");
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
    assert_eq!(
        (
            machine.lease_capacity,
            machine.lease_bytes,
            machine.lease_root_bytes
        ),
        (0, 0, 0)
    );
    assert_eq!(machine.metrics.retained_lease_capacity, 0);
    assert_eq!(machine.metrics.retained_lease_bytes, 0);
    assert_eq!(machine.metrics.retained_lease_root_bytes, 0);
    assert_eq!((machine.argument_capacity, machine.argument_bytes), (0, 0));
    assert_eq!(machine.string_payload_bytes, 0);
    assert_eq!(machine.metrics.retained_string_payload_bytes, 0);
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
fn production_borrowed_result_rejects_oversized_external_buffer_before_body_effects() {
    // Safe shallow production RED: this also compiles on the exact preceding
    // commit, without the new result role, wrapper metrics or dispatch modes.
    let source = r#"
fn Read(&text: str, n: int): int! {
    if (n <= 0) { return ok(text.len()) }
    return ok(Read(text, n - 1)?)
}
"#;
    let mut text = String::with_capacity(Limits::default().values.max_storage_bytes + 1);
    text.push_str("\u{4e2d}\u{e9}\0");
    let pointer = text.as_ptr();
    let capacity = text.capacity();
    let span = Span::default();
    let mut owner = Env::new();
    owner
        .define_owned("text".into(), Value::String(text), false, span)
        .unwrap();
    let (view, lease) = owner.borrow("text", span).unwrap();
    assert!(
        lease.is_some(),
        "the real external owner retains its restoring lease"
    );
    let mut interpreter = production_interpreter(source);
    let error = interpreter
        .call_function("Read", vec![view, Value::Int(4)], span, 0)
        .expect_err("the actual external buffer capacity must reject before body effects");
    assert!(error
        .message
        .contains("internal execution storage rejected"));
    assert_eq!(interpreter.steps, 0);
    assert_eq!(interpreter.call_depth, 0);
    owner
        .with_value("text", span, |value| {
            let Value::Borrowed(view) = value else {
                panic!("the external restoring lease must remain alive")
            };
            view.with_read(span, |value| {
                let Value::String(text) = value else {
                    unreachable!()
                };
                assert_eq!((text.as_ptr(), text.capacity()), (pointer, capacity));
                assert_eq!(text, "\u{4e2d}\u{e9}\0");
                Ok(())
            })
        })
        .unwrap();
    drop(lease);
    owner
        .with_value("text", span, |value| {
            let Value::String(text) = value else {
                panic!("the original external owner must restore")
            };
            assert_eq!((text.as_ptr(), text.capacity()), (pointer, capacity));
            assert_eq!(text, "\u{4e2d}\u{e9}\0");
            Ok(())
        })
        .unwrap();
}

#[test]
fn borrowed_primitive_result_keeps_external_owner_and_temporary_root_through_512_calls() {
    assert_eq!(MAX_CALL_DEPTH, 512);
    for external in [false, true] {
        for chunk in [1, 7, 256] {
            let mut machine = machine(READ, Limits::default());
            let mut host = production_interpreter(READ);
            let (text, pointer, capacity) = text_input();
            let mut owner = Env::new();
            owner
                .define_owned("text".into(), Value::String(text), false, Span::default())
                .unwrap();
            let (value, lease) = if external {
                owner.borrow("text", Span::default()).unwrap()
            } else {
                (
                    owner.take_owned_string("text", Span::default()).unwrap(),
                    None,
                )
            };
            let mut args = vec![value, Value::Int(511)];
            let before_roots = crate::value::borrow_roots_created();
            machine
                .start_owned_once("Read", &mut args, 0, false)
                .unwrap();
            let observed = observed_view(&machine, "text");
            observed
                .with_read(Span::default(), |value| {
                    check_text(value, pointer, capacity)
                })
                .unwrap();
            assert_eq!(
                finish(&mut machine, &mut host, chunk).unwrap(),
                Value::Result {
                    ok: true,
                    value: Box::new(Value::Int(3))
                }
            );
            assert_eq!(
                (machine.metrics.entered_calls, machine.metrics.peak_calls),
                (512, 512)
            );
            assert_eq!(machine.metrics.lease_roots_created, usize::from(!external));
            assert_eq!(
                crate::value::borrow_roots_created() - before_roots,
                usize::from(!external)
            );
            assert_eq!(
                machine.metrics.peak_lease_root_bytes,
                usize::from(!external) * std::mem::size_of::<Value>()
            );
            assert_eq!(machine.metrics.peak_string_payload_bytes, capacity);
            assert_eq!(machine.metrics.peak_result_wrapper_capacity, 1);
            assert_eq!(
                machine.metrics.peak_result_wrapper_bytes,
                std::mem::size_of::<Value>()
            );
            assert!(args.is_empty() && args.capacity() == 0);
            drained(&machine);
            if external {
                check_owner(&owner, pointer, capacity, true);
                machine.prepare_next_run().unwrap();
                let (view, nested) = owner.borrow("text", Span::default()).unwrap();
                assert!(nested.is_none(), "reborrow cannot create another root");
                let mut args = vec![view, Value::Int(0)];
                machine
                    .start_owned_once("Read", &mut args, 0, false)
                    .unwrap();
                assert_eq!(
                    finish(&mut machine, &mut host, chunk).unwrap(),
                    Value::Result {
                        ok: true,
                        value: Box::new(Value::Int(3))
                    }
                );
                drained(&machine);
                drop(lease);
                check_owner(&owner, pointer, capacity, false);
            } else {
                assert!(observed
                    .with_read(Span::default(), |_| Ok(()))
                    .unwrap_err()
                    .message
                    .contains("outlived"));
                owner
                    .with_value("text", Span::default(), |value| {
                        assert!(matches!(value, Value::Null));
                        Ok(())
                    })
                    .unwrap();
            }
            assert_eq!(host.call_depth, 0);
        }
    }
}

#[test]
fn production_borrowed_result_accepts_512_and_rejects_first_513_without_replay() {
    for n in [511, 512] {
        let (owner, view, lease, pointer, capacity) = root_view();
        let mut host = production_interpreter(READ);
        assert!(code::eligible_borrowed_string_named(
            &host.functions,
            &host.functions["Read"],
            Limits::default()
        ));
        let actual = host.call_function("Read", vec![view, Value::Int(n)], Span::default(), 0);
        if n == 511 {
            assert_eq!(
                actual.unwrap(),
                Value::Result {
                    ok: true,
                    value: Box::new(Value::Int(3))
                }
            );
        } else {
            assert_eq!(
                actual.unwrap_err().code.as_deref(),
                Some("call_depth_exceeded")
            );
        }
        assert_eq!(host.call_depth, 0);
        check_owner(&owner, pointer, capacity, true);
        drop(lease);
        check_owner(&owner, pointer, capacity, false);
        let mut machine = machine(READ, Limits::default());
        let (owner, view, lease, pointer, capacity) = root_view();
        let mut args = vec![view, Value::Int(n)];
        machine
            .start_owned_once("Read", &mut args, 0, false)
            .unwrap();
        let actual = finish(&mut machine, &mut host, 1);
        if n == 511 {
            assert!(matches!(actual, Ok(Value::Result { ok: true, .. })));
        } else {
            assert_eq!(
                actual.unwrap_err().code.as_deref(),
                Some("call_depth_exceeded")
            );
        }
        assert_eq!(machine.metrics.entered_calls, 512);
        assert_eq!(machine.metrics.peak_calls, 512);
        assert_eq!(
            machine.metrics.peak_result_wrapper_capacity,
            usize::from(n == 511)
        );
        drained(&machine);
        drop(lease);
        check_owner(&owner, pointer, capacity, false);
    }
}

#[test]
fn borrowed_result_propagation_frees_previous_box_before_reboxing_under_exact_budget() {
    eprintln!("borrowed primitive Result Box backing={} (compiler sizeof<Value>, allocator metadata and RSS excluded)", std::mem::size_of::<Value>());
    let mut machine = machine(READ, Limits::default());
    let mut host = production_interpreter(READ);
    let (owner, view, lease, pointer, capacity) = root_view();
    let mut args = vec![view, Value::Int(1)];
    machine
        .start_owned_once("Read", &mut args, 0, false)
        .unwrap();
    pause_at(&mut machine, &mut host, |machine| {
        machine.pending.is_none()
            && machine.exits.len() == 0
            && matches!(top_work(machine), Some(Work::Propagate(_)))
    });
    assert_eq!(wrapper_count(&machine), 1);
    machine.value_limits.max_frames = live_value_count(&machine);
    machine.value_limits.max_storage_bytes = live_value_bytes(&machine);
    assert!(matches!(
        machine
            .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut host)
            .unwrap(),
        ChunkProgress::More { popped: 1 }
    ));
    assert_eq!(
        wrapper_count(&machine),
        0,
        "'?' moves out and frees the genuine successful Box before scalar emit"
    );
    assert!(matches!(
        machine.values.get(machine.values.len() - 1),
        Some(Value::Int(3))
    ));
    pause_at(&mut machine, &mut host, at_wrap);
    assert_eq!(wrapper_count(&machine), 0);
    assert_eq!(
        finish(&mut machine, &mut host, 1).unwrap(),
        Value::Result {
            ok: true,
            value: Box::new(Value::Int(3))
        }
    );
    assert_eq!(machine.metrics.peak_result_wrapper_capacity, 1);
    drained(&machine);
    check_owner(&owner, pointer, capacity, true);
    drop(lease);
    check_owner(&owner, pointer, capacity, false);
}

#[test]
fn borrowed_result_wrapper_count_and_byte_failures_follow_operand_evaluation_and_restore_input() {
    for count_rejection in [false, true] {
        let mut machine = machine(READ, Limits::default());
        let mut host = production_interpreter(READ);
        let (owner, view, lease, pointer, capacity) = root_view();
        let mut args = vec![view, Value::Int(0)];
        machine
            .start_owned_once("Read", &mut args, 0, false)
            .unwrap();
        pause_at(&mut machine, &mut host, at_wrap);
        assert!(host.steps > 0);
        assert!(matches!(
            machine.values.get(machine.values.len() - 1),
            Some(Value::Int(3))
        ));
        assert_eq!(wrapper_count(&machine), 0);
        if count_rejection {
            machine.value_limits.max_frames = live_value_count(&machine);
        } else {
            machine.value_limits.max_storage_bytes =
                live_value_bytes(&machine) + std::mem::size_of::<Value>() - 1;
        }
        let error = finish(&mut machine, &mut host, 1).unwrap_err();
        assert!(error.message.contains(if count_rejection {
            "FrameLimit"
        } else {
            "StorageLimit"
        }));
        assert_eq!(machine.metrics.peak_result_wrapper_capacity, 0);
        assert_eq!(machine.metrics.peak_result_wrapper_bytes, 0);
        check_owner(&owner, pointer, capacity, true);
        drained(&machine);
        machine.value_limits = Limits::default().values;
        machine.prepare_next_run().unwrap();
        let (view, nested) = owner.borrow("text", Span::default()).unwrap();
        assert!(nested.is_none());
        let mut args = vec![view, Value::Int(0)];
        machine
            .start_owned_once("Read", &mut args, 0, false)
            .unwrap();
        assert_eq!(
            finish(&mut machine, &mut host, 7).unwrap(),
            Value::Result {
                ok: true,
                value: Box::new(Value::Int(3))
            }
        );
        drained(&machine);
        drop(lease);
        check_owner(&owner, pointer, capacity, false);
    }
}

#[test]
fn borrowed_result_lease_root_budget_and_partial_binding_failures_retire_actual_inputs() {
    for count_rejection in [false, true] {
        let (text, _, capacity) = text_input();
        let mut args = vec![Value::String(text), Value::Int(0)];
        let mut limits = Limits::default();
        if count_rejection {
            limits.values.max_frames = 2 * args.capacity() + 1;
        } else {
            limits.values.max_storage_bytes = capacity
                + 2 * args.capacity() * std::mem::size_of::<Value>()
                + std::mem::size_of::<BorrowLease>()
                + std::mem::size_of::<Value>()
                - 1;
        }
        let mut machine = machine(READ, limits);
        let mut host = production_interpreter(READ);
        let before_roots = crate::value::borrow_roots_created();
        machine
            .start_owned_once("Read", &mut args, 0, false)
            .unwrap();
        let error = finish(&mut machine, &mut host, 1).unwrap_err();
        assert!(error
            .message
            .contains("internal execution storage rejected"));
        assert_eq!(host.steps, 0);
        assert_eq!(
            machine.metrics.lease_roots_created, 0,
            "root admission rejects before real BorrowLease::temporary allocation"
        );
        assert_eq!(crate::value::borrow_roots_created(), before_roots);
        assert_eq!(machine.metrics.peak_lease_root_bytes, 0);
        assert_eq!(machine.metrics.peak_result_wrapper_capacity, 0);
        assert!(
            args.is_empty() && args.capacity() == 0,
            "the Vec transferred before this internal parameter reservation failure"
        );
        drained(&machine);
    }
    const PARTIAL: &str =
        "fn Read(&first: str, &second: str): int! { return ok(first.len() + second.len()) }";
    let mut machine = machine(PARTIAL, Limits::default());
    let mut host = production_interpreter(PARTIAL);
    let (text, pointer, capacity) = text_input();
    let mut args = vec![Value::String(text), Value::Int(0)];
    // Internal failure only: the checked function has two read-view String
    // parameters, while this private kernel deliberately supplies a primitive.
    machine
        .start_owned_once("Read", &mut args, 0, false)
        .unwrap();
    let observed = observed_view(&machine, "first");
    observed
        .with_read(Span::default(), |value| {
            check_text(value, pointer, capacity)
        })
        .unwrap();
    assert_eq!(machine.metrics.lease_roots_created, 1);
    assert_eq!(machine.lease_root_bytes, std::mem::size_of::<Value>());
    let error = finish(&mut machine, &mut host, 1).unwrap_err();
    assert!(error.message.contains("does not support"));
    assert_eq!(machine.metrics.entered_calls, 1);
    assert_eq!(host.steps, 0);
    assert!(args.is_empty());
    assert!(observed
        .with_read(Span::default(), |_| Ok(()))
        .unwrap_err()
        .message
        .contains("outlived"));
    drained(&machine);
}

#[test]
fn borrowed_result_later_argument_failure_does_not_enter_child_or_create_wrapper() {
    const SOURCE: &str = r#"
fn Read(&text: str, n: int): int! {
    if (n <= 0) { return ok(text.len()) }
    return ok(Read(text, 1 / 0)?)
}
"#;
    let mut machine = machine(SOURCE, Limits::default());
    let mut host = production_interpreter(SOURCE);
    let (text, pointer, capacity) = text_input();
    let mut args = vec![Value::String(text), Value::Int(1)];
    machine
        .start_owned_once("Read", &mut args, 0, false)
        .unwrap();
    let observed = observed_view(&machine, "text");
    observed
        .with_read(Span::default(), |value| {
            check_text(value, pointer, capacity)
        })
        .unwrap();
    let error = finish(&mut machine, &mut host, 1).unwrap_err();
    assert_eq!(error.message, "division by zero");
    assert_eq!(machine.metrics.entered_calls, 1);
    assert_eq!(machine.metrics.lease_roots_created, 1);
    assert_eq!(machine.metrics.peak_string_payload_bytes, capacity);
    assert_eq!(machine.metrics.peak_result_wrapper_capacity, 0);
    assert!(observed
        .with_read(Span::default(), |_| Ok(()))
        .unwrap_err()
        .message
        .contains("outlived"));
    assert!(host.caller_owners.is_empty());
    drained(&machine);
}

#[test]
fn borrowed_result_preserves_expired_and_foreign_view_errors_before_effects() {
    let (owner, view, lease, pointer, capacity) = root_view();
    drop(lease);
    let mut host = production_interpreter(READ);
    let mut args = vec![view, Value::Int(0)];
    let entry = host.functions["Read"].clone();
    let error = try_execute_borrowed_string(&mut host, &entry, &mut args, 0)
        .expect("a fully admitted expired root must return its original error")
        .err()
        .expect("the expired root must fail before executing the body");
    assert_eq!(
        error.message,
        "borrowed value outlived its synchronous call"
    );
    assert_eq!(host.steps, 0);
    assert_eq!(
        args.len(),
        2,
        "expired inspection must not consume the original input Vec"
    );
    check_owner(&owner, pointer, capacity, false);
    let (owner, view, lease, pointer, capacity) = root_view();
    let (message, steps, depth) = std::thread::scope(|scope| {
        scope
            .spawn(move || {
                let mut host = production_interpreter(READ);
                let error = host
                    .call_function("Read", vec![view, Value::Int(0)], Span::default(), 0)
                    .unwrap_err();
                (error.message, host.steps, host.call_depth)
            })
            .join()
            .expect("the finite foreign read must return its typed error")
    });
    assert_eq!(
        message,
        "borrowed value cannot be read across threads or async tasks"
    );
    assert_eq!((steps, depth), (0, 0));
    check_owner(&owner, pointer, capacity, true);
    drop(lease);
    check_owner(&owner, pointer, capacity, false);
}

#[test]
fn borrowed_result_static_primitive_helper_and_other_primitive_payloads_keep_input_readonly() {
    for (source, expected) in [
        ("fn Add(n: int): int { return n + 1 }\nfn Read(&text: str): int! { return ok(Add(text.len())) }", Value::Int(4)),
        ("fn Read(&text: str): float! { return ok(1.5) }", Value::Float(1.5)),
        ("fn Read(&text: str): bool! { return ok(text.len() > 0) }", Value::Bool(true)),
        ("fn Read(&text: str): null! { return ok(null) }", Value::Null),
    ] {
        let mut machine = machine(source, Limits::default());
        let mut host = production_interpreter(source);
        let (owner, view, lease, pointer, capacity) = root_view();
        let mut args = vec![view];
        machine.start_owned_once("Read", &mut args, 0, false).unwrap();
        assert_eq!(finish(&mut machine, &mut host, 7).unwrap(), Value::Result { ok: true, value: Box::new(expected) });
        assert_eq!(machine.metrics.peak_result_wrapper_capacity, 1);
        assert_eq!(machine.metrics.peak_result_wrapper_bytes, std::mem::size_of::<Value>());
        drained(&machine);
        check_owner(&owner, pointer, capacity, true);
        drop(lease);
        check_owner(&owner, pointer, capacity, false);
    }
}

#[test]
fn private_borrowed_result_propagation_kernel_preserves_type_error_and_failure_payload() {
    for injected_failure in [false, true] {
        let mut machine = machine(READ, Limits::default());
        let mut host = production_interpreter(READ);
        let (owner, view, lease, pointer, capacity) = root_view();
        let mut args = vec![view, Value::Int(1)];
        machine
            .start_owned_once("Read", &mut args, 0, false)
            .unwrap();
        pause_at(&mut machine, &mut host, |machine| {
            machine.pending.is_none()
                && machine.exits.len() == 0
                && matches!(top_work(machine), Some(Work::Propagate(_)))
        });
        assert_eq!(wrapper_count(&machine), 1);
        // These are private kernel fault probes, not source admission of err
        // or ill-typed '?'. The true branch edits the already allocated Box;
        // it does not synthesize a second owner or manipulate charge counters.
        let index = machine.values.len() - 1;
        let operand = machine.values.get_mut(index).unwrap();
        if injected_failure {
            let Value::Result { ok, value } = operand else {
                unreachable!()
            };
            *ok = false;
            **value = Value::Int(17);
        } else {
            *operand = Value::Int(17); // Real assignment drops the original Box.
        }
        assert!(matches!(
            machine
                .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut host)
                .unwrap(),
            ChunkProgress::More { popped: 1 }
        ));
        assert_eq!(wrapper_count(&machine), 0);
        if injected_failure {
            assert!(matches!(
                &machine.pending,
                Some(Ok(Flow::Fail {
                    value: Value::Int(17),
                    ..
                }))
            ));
            assert_eq!(
                finish(&mut machine, &mut host, 1).unwrap(),
                Value::Result {
                    ok: false,
                    value: Box::new(Value::Int(17))
                }
            );
        } else {
            assert_eq!(
                finish(&mut machine, &mut host, 1).unwrap_err().message,
                "'?' expects result but got int"
            );
        }
        drained(&machine);
        drop(lease);
        check_owner(&owner, pointer, capacity, false);
    }
}

#[test]
fn native_borrowed_result_kernel_abandonment_drops_real_root_lease_and_restores_tls() {
    let observed = std::cell::RefCell::new(None);
    let native = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut machine = machine(READ, Limits::default());
        let mut host = production_interpreter(READ);
        let (text, pointer, capacity) = text_input();
        let mut args = vec![Value::String(text), Value::Int(0)];
        machine
            .start_owned_once("Read", &mut args, 0, false)
            .unwrap();
        let view = observed_view(&machine, "text");
        view.with_read(Span::default(), |value| {
            check_text(value, pointer, capacity)
        })
        .unwrap();
        *observed.borrow_mut() = Some(view);
        pause_at(&mut machine, &mut host, |machine| {
            machine.result_wrapper_capacity == 1
        });
        assert_eq!(wrapper_count(&machine), 1);
        assert_eq!(machine.lease_root_bytes, std::mem::size_of::<Value>());
        assert_eq!(machine.string_payload_bytes, capacity);
        let context = RuntimeTermination {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: Instant::now(),
        };
        let _guard = CleanupGuard::enter(context);
        // Native kernel ownership ends before the actual TLS guard. This does
        // not claim a source-level destructor or fabricate a Task retirement ACK.
        let _abandoned = machine;
        panic!("actual borrowed Result kernel abandonment");
    }));
    let panic = native.expect_err("the real kernel panic must escape");
    assert_eq!(
        panic.downcast_ref::<&str>().copied(),
        Some("actual borrowed Result kernel abandonment")
    );
    let view = observed
        .into_inner()
        .expect("the weak parameter was observed before native abandonment");
    assert_eq!(
        view.with_read(Span::default(), |_| Ok(()))
            .unwrap_err()
            .message,
        "borrowed value outlived its synchronous call"
    );
    assert!(current_cleanup_context().is_none());
    assert!(crate::runtime::task::current_execution_termination().is_none());
    let mut host = production_interpreter(READ);
    let (text, _, _) = text_input();
    assert_eq!(
        host.call_function(
            "Read",
            vec![Value::String(text), Value::Int(4)],
            Span::default(),
            0
        )
        .unwrap(),
        Value::Result {
            ok: true,
            value: Box::new(Value::Int(3))
        }
    );
    assert_eq!(host.call_depth, 0);
}

#[test]
fn unsupported_borrowed_result_trees_and_role_caches_leave_the_external_input_unmoved() {
    for source in [
        "fn Read(&text: str, n: int): int! { result = ok(text.len())\nreturn result }",
        "fn Pass(value: int!): int! { return value }\nfn Read(&text: str, n: int): int! { return ok(Pass(ok(text.len()))?) }",
        "fn Read(&text: str, n: int): int! { if (n <= 0) { return ok(text.len()) }\nreturn Read(text, n - 1) }",
        "fn Read(&text: str, n: int): int! { return err(\"expected\") }",
        "fn Read(&text: str, n: int): int! { try { marker = 1 } finally { marker = 2 }\nreturn ok(text.len()) }",
        "fn Read(&text: str, n: int): int! { try { marker = 1 } catch (e) { marker = 2 }\nreturn ok(text.len()) }",
        "fn Leaf(&text: str, n: int): int! { try { marker = 1 } finally { marker = 2 }\nreturn ok(text.len()) }\nfn Read(&text: str, n: int): int! { return ok(Leaf(text, n)?) }",
        "fn Helper(&text: str, n: int): int { try { return Read(text, n - 1)? } finally { marker = 1 }\nreturn 0 }\nfn Read(&text: str, n: int): int! { if (n <= 0) { return ok(text.len()) }\nreturn ok(Helper(text, n)) }",
        "fn Read(&text: str, n: int): int! { fn Local(value: int): int { return value + 1 }\nreturn ok(Local(text.len())) }",
        "async fn Child(): int! { return ok(1) }\nfn Read(&text: str, n: int): int! { child = Child()\nreturn ok(text.len()) }",
        "fn Read(&text: str, extra: str): int! { return ok(text.len() + extra.len()) }",
    ] {
        let mut host = production_interpreter(source);
        let entry = host.functions["Read"].clone();
        assert!(!code::eligible_borrowed_string_named(&host.functions, &entry, Limits::default()), "{source}");
        assert!(matches!(CodeHandle::compile_borrowed_string_named(&host.functions, &entry, Limits::default()), Err(AdmissionError::Unsupported(_))), "{source}");
        let (owner, view, lease, pointer, capacity) = root_view();
        let mut args = vec![view, if source.contains("extra: str") { Value::String(String::from("extra")) } else { Value::Int(1) }];
        let before_roots = crate::value::borrow_roots_created();
        assert!(try_execute_borrowed_string(&mut host, &entry, &mut args, 0).is_none(), "{source}");
        assert!(try_execute_owned(&mut host, &entry, &mut args, 0).is_none());
        assert_eq!(args.len(), 2);
        assert_eq!(host.steps, 0);
        assert_eq!(host.call_depth, 0);
        assert_eq!(crate::value::borrow_roots_created(), before_roots);
        assert!(host.task_runtime.is_none(), "whole-tree rejection must not start an async child");
        check_text(&args[0], pointer, capacity).unwrap();
        check_owner(&owner, pointer, capacity, true);
        drop(args);
        drop(lease);
        check_owner(&owner, pointer, capacity, false);
    }
    let source = "async fn Read(&text: str): int! { return ok(text.len()) }\nfn main() {}";
    let program = Parser::new(Lexer::new(source).tokenize().unwrap())
        .parse_program()
        .unwrap();
    assert!(
        crate::checker::Checker::new()
            .check(&program)
            .unwrap_err()
            .message
            .contains("borrow"),
        "illegal async view declaration must keep its original checker rejection"
    );
}
