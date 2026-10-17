use super::*;

const READ: &str = r#"
fn Read(&text: str, n: int): int {
    if (n <= 0) { return text.len() + text.byte_len() }
    return Read(text, n - 1)
}
fn Entry(text: str, n: int): str {
    observed = Read(text, n)
    return text
}
"#;

fn text_input() -> (String, *const u8, usize) {
    let mut text = String::with_capacity(4096);
    text.push_str("\u{4e2d}\u{e9}\0");
    let pointer = text.as_ptr();
    let capacity = text.capacity();
    (text, pointer, capacity)
}

fn drained_leases(machine: &Machine) {
    assert_eq!(machine.metrics.retained_lease_capacity, 0);
    assert_eq!(machine.metrics.retained_lease_bytes, 0);
    assert_eq!(machine.metrics.retained_lease_root_bytes, 0);
    assert_eq!(
        (
            machine.lease_capacity,
            machine.lease_bytes,
            machine.lease_root_bytes
        ),
        (0, 0, 0)
    );
    assert_eq!((machine.argument_capacity, machine.argument_bytes), (0, 0));
    assert_eq!(machine.string_payload_bytes, 0);
    assert_drained(&machine.metrics, 0);
}

#[test]
fn owned_entry_borrows_once_and_restores_the_original_string_after_512_calls() {
    let source = functions(READ);
    let code =
        CodeHandle::compile_borrowed_string_named(&source, &source["Entry"], Limits::default())
            .unwrap();
    let mut machine = Machine::from_code(code, Limits::default(), 0);
    let (text, pointer, capacity) = text_input();
    let mut args = vec![Value::String(text), Value::Int(510)];
    let before_roots = crate::value::borrow_roots_created();
    let mut interpreter = production_interpreter(READ);
    let flow = machine
        .run_owned_with_hooks("Entry", &mut args, 0, false, &mut interpreter)
        .unwrap();
    let Value::String(text) = flow.into_call_value(Span::default(), "function").unwrap() else {
        panic!("the real caller owner must return after lease restoration");
    };
    assert_eq!(text.as_ptr(), pointer);
    assert_eq!(text.capacity(), capacity);
    assert_eq!(text, "\u{4e2d}\u{e9}\0");
    assert_eq!(machine.metrics.entered_calls, 512);
    assert_eq!(machine.metrics.peak_calls, 512);
    assert_eq!(crate::value::borrow_roots_created() - before_roots, 1);
    assert_eq!(machine.metrics.lease_roots_created, 1);
    assert!(machine.metrics.peak_lease_capacity > 0);
    assert!(machine.metrics.peak_lease_bytes > 0);
    assert!(machine.metrics.peak_lease_root_bytes > 0);
    assert_eq!(machine.metrics.peak_string_payload_bytes, capacity);
    drained_leases(&machine);
    let (text, _, _) = text_input();
    assert_eq!(
        interpreter
            .call_function(
                "Entry",
                vec![Value::String(text), Value::Int(5)],
                Span::default(),
                0
            )
            .unwrap(),
        Value::String("\u{4e2d}\u{e9}\0".into())
    );
}

#[test]
fn borrowed_root_reborrow_keeps_the_external_lease_and_unicode_length_contract() {
    let source = functions(READ);
    let code =
        CodeHandle::compile_borrowed_string_named(&source, &source["Read"], Limits::default())
            .unwrap();
    let mut machine = Machine::from_code(code, Limits::default(), 0);
    let (text, pointer, capacity) = text_input();
    let span = Span::default();
    let mut env = Env::new();
    env.define_owned("text".into(), Value::String(text), false, span)
        .unwrap();
    let (view, lease) = env.borrow("text", span).unwrap();
    let before_roots = crate::value::borrow_roots_created();
    let mut args = vec![view, Value::Int(511)];
    let mut interpreter = production_interpreter(READ);
    let result = machine
        .run_owned_with_hooks("Read", &mut args, 0, false, &mut interpreter)
        .unwrap()
        .into_call_value(span, "function")
        .unwrap();
    assert_eq!(result, Value::Int(9));
    assert_eq!(machine.metrics.entered_calls, 512);
    assert_eq!(machine.metrics.lease_roots_created, 0);
    assert_eq!(crate::value::borrow_roots_created(), before_roots);
    drop(lease);
    env.with_value("text", span, |value| {
        let Value::String(text) = value else {
            panic!("original external binding must restore");
        };
        assert_eq!(text.as_ptr(), pointer);
        assert_eq!(text.capacity(), capacity);
        Ok(())
    })
    .unwrap();
    drained_leases(&machine);
}

const ARGUMENT_FAILURE: &str = r#"
fn Read(&text: str, n: int): int { return text.len() + n }
fn Later(): int { return 77 }
fn Entry(text: str): int {
    answer = Read(text, 1 / 0)
    return Later()
}
"#;
const SIMPLE_LOAN: &str = r#"
fn Read(&text: str, n: int): int { return text.len() + n }
fn Entry(text: str): int { return Read(text, 7) }
"#;
const MAX_LOAN_CHECKPOINTS: usize = 10_000;

fn loan_machine(source: &str) -> Machine {
    let source = functions(source);
    let code =
        CodeHandle::compile_borrowed_string_named(&source, &source["Entry"], Limits::default())
            .unwrap();
    Machine::from_code(code, Limits::default(), 0)
}

fn top_work(machine: &Machine) -> Option<&Work> {
    machine
        .work
        .len()
        .checked_sub(1)
        .and_then(|index| machine.work.get(index))
}

fn pause_loan_at(machine: &mut Machine, host: &mut Interpreter, ready: impl Fn(&Machine) -> bool) {
    for _ in 0..MAX_LOAN_CHECKPOINTS {
        if ready(machine) {
            assert_eq!(machine.state, MachineState::Running);
            return;
        }
        match machine
            .dispatch_chunk(NonZeroUsize::new(1).unwrap(), host)
            .unwrap()
        {
            ChunkProgress::More { popped } => assert_eq!(popped, 1),
            ChunkProgress::Waiting { .. } => panic!("String loan fixture has no Task wait"),
            ChunkProgress::Finished { result, .. } => {
                if let Err(error) = result {
                    panic!("loan checkpoint was not reached: {}", error.message);
                }
                panic!("loan fixture finished before its real checkpoint");
            }
        }
    }
    panic!("finite String loan checkpoint bound exhausted");
}

fn finish_loan(machine: &mut Machine, host: &mut Interpreter) -> KuResult<Flow> {
    for _ in 0..MAX_LOAN_CHECKPOINTS {
        match machine
            .dispatch_chunk(NonZeroUsize::new(256).unwrap(), host)
            .unwrap()
        {
            ChunkProgress::More { .. } => {}
            ChunkProgress::Waiting { .. } => panic!("String loan fixture has no Task wait"),
            ChunkProgress::Finished { result, .. } => return result,
        }
    }
    panic!("finite String loan completion bound exhausted");
}

fn assert_binding_buffer(env: &Env, pointer: *const u8, capacity: usize, borrowed: bool) {
    env.with_value("text", Span::default(), |value| {
        let check = |value: &Value| {
            let Value::String(text) = value else {
                panic!("real String root must remain intact");
            };
            assert_eq!(text.as_ptr(), pointer);
            assert_eq!(text.capacity(), capacity);
            assert_eq!(text, "\u{4e2d}\u{e9}\0");
            Ok(())
        };
        if borrowed {
            let Value::Borrowed(view) = value else {
                panic!("actual Env.borrow must replace the binding with a view");
            };
            assert!(view.is_root_view());
            view.with_read(Span::default(), check)
        } else {
            check(value)
        }
    })
    .unwrap();
}

#[test]
fn the_first_excess_borrowed_call_retires_every_frame_and_restoring_lease() {
    let source = functions(READ);
    let code =
        CodeHandle::compile_borrowed_string_named(&source, &source["Entry"], Limits::default())
            .unwrap();
    let mut machine = Machine::from_code(code, Limits::default(), 0);
    let (text, _, capacity) = text_input();
    // Entry plus Read(511)..Read(0) attempts the first, 513th active call.
    let mut args = vec![Value::String(text), Value::Int(511)];
    let before_roots = crate::value::borrow_roots_created();
    let mut host = production_interpreter(READ);
    let error = machine
        .run_owned_with_hooks("Entry", &mut args, 0, false, &mut host)
        .err()
        .expect("the existing 512-call limit must reject the next real call");
    assert_eq!(error.code.as_deref(), Some("call_depth_exceeded"));
    assert_eq!(
        (machine.metrics.entered_calls, machine.metrics.peak_calls),
        (512, 512)
    );
    assert_eq!(machine.metrics.lease_roots_created, 1);
    assert_eq!(crate::value::borrow_roots_created() - before_roots, 1);
    assert_eq!(machine.metrics.peak_string_payload_bytes, capacity);
    assert!(args.is_empty() && args.capacity() == 0);
    assert_eq!(host.call_depth, 0);
    drained_leases(&machine);
}

#[test]
fn later_primitive_argument_error_and_fuel_restore_the_first_actual_string_loan() {
    for fuel_failure in [false, true] {
        let mut machine = loan_machine(ARGUMENT_FAILURE);
        let (text, pointer, capacity) = text_input();
        let mut args = vec![Value::String(text)];
        let mut host = production_interpreter(ARGUMENT_FAILURE);
        let before_roots = crate::value::borrow_roots_created();
        machine
            .start_owned_once("Entry", &mut args, 0, fuel_failure)
            .unwrap();
        pause_loan_at(&mut machine, &mut host, |machine| {
            machine.metrics.lease_roots_created == 1
        });
        assert_eq!(
            machine.calls.len(),
            1,
            "the callee has not entered before all arguments succeed"
        );
        assert!(machine.lease_capacity > 0 && machine.lease_root_bytes > 0);
        // Clone only the actual Env binding cells. This retains the caller's
        // original cell for observation, without cloning its String payload.
        let caller = machine.calls.get(0).unwrap().env.clone();
        assert_binding_buffer(&caller, pointer, capacity, true);
        if fuel_failure {
            machine.remaining_dispatches = 0;
        }
        let error = finish_loan(&mut machine, &mut host)
            .err()
            .expect("the original argument failure must survive cleanup");
        assert_eq!(
            error.message,
            if fuel_failure {
                "internal execution prototype test fuel exhausted"
            } else {
                "division by zero"
            }
        );
        assert_eq!(
            machine.metrics.entered_calls, 1,
            "neither Read nor Later may execute"
        );
        assert_eq!(machine.metrics.lease_roots_created, 1);
        assert_eq!(crate::value::borrow_roots_created() - before_roots, 1);
        assert_binding_buffer(&caller, pointer, capacity, false);
        assert!(args.is_empty());
        assert!(current_cleanup_context().is_none());
        drained_leases(&machine);
    }
}

#[test]
fn native_kernel_abandonment_restores_the_borrow_under_the_same_short_cleanup_deadline() {
    let past = Instant::now();
    let ambient = RuntimeTermination {
        reason: TerminationReason::TimedOut,
        cleanup_deadline: past,
    };
    let shorter = RuntimeTermination {
        reason: TerminationReason::Cancelled,
        cleanup_deadline: past.checked_sub(Duration::from_millis(1)).unwrap(),
    };
    let retained = std::cell::RefCell::new(None);
    let (text, pointer, capacity) = text_input();
    let before_roots = crate::value::borrow_roots_created();
    let native = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Native kernel abandonment, not a language callback or expanded
        // Owned admission. This declaration order makes Machine drop before
        // the real ambient guard restores TLS during the native unwind.
        let _guard = CleanupGuard::enter(ambient);
        let mut machine = loan_machine(SIMPLE_LOAN);
        let mut args = vec![Value::String(text)];
        let mut host = production_interpreter(SIMPLE_LOAN);
        machine
            .start_owned_once("Entry", &mut args, 0, false)
            .unwrap();
        pause_loan_at(&mut machine, &mut host, |machine| {
            machine.metrics.lease_roots_created == 1
        });
        let caller = machine.calls.get(0).unwrap().env.clone();
        assert_binding_buffer(&caller, pointer, capacity, true);
        assert!(machine.lease_root_bytes > 0);
        *retained.borrow_mut() = Some(caller);
        drop(CleanupGuard::enter(shorter));
        let effective = current_cleanup_context().expect("live native cleanup window");
        assert_eq!(effective.reason, ambient.reason);
        assert_eq!(effective.cleanup_deadline, shorter.cleanup_deadline);
        panic!("actual String-loan kernel abandonment");
    }));
    let panic = match native {
        Err(panic) => panic,
        Ok(_) => panic!("the actual kernel panic must escape"),
    };
    assert_eq!(
        panic.downcast_ref::<&str>().copied(),
        Some("actual String-loan kernel abandonment")
    );
    assert!(current_cleanup_context().is_none());
    assert!(crate::runtime::task::current_execution_termination().is_none());
    let caller = retained
        .into_inner()
        .expect("the actual caller cell was captured before abandonment");
    assert_binding_buffer(&caller, pointer, capacity, false);
    assert_eq!(crate::value::borrow_roots_created() - before_roots, 1);
    let mut healthy = production_interpreter(SIMPLE_LOAN);
    let (text, _, _) = text_input();
    assert_eq!(
        healthy
            .call_function("Entry", vec![Value::String(text)], Span::default(), 0)
            .unwrap(),
        Value::Int(10)
    );
    assert_eq!(healthy.call_depth, 0);
    assert!(current_cleanup_context().is_none());
}

fn live_loan_bytes(machine: &Machine) -> usize {
    machine.values.charged_storage_bytes()
        + machine.argument_bytes
        + machine.exit_handle_bytes
        + machine.string_payload_bytes
        + machine.lease_bytes
        + machine.lease_root_bytes
}

#[test]
fn lease_vector_and_root_value_budgets_reject_before_the_actual_env_borrow() {
    for root_value in [false, true] {
        let mut machine = loan_machine(SIMPLE_LOAN);
        let (text, pointer, capacity) = text_input();
        let mut args = vec![Value::String(text)];
        let mut host = production_interpreter(SIMPLE_LOAN);
        let before_roots = crate::value::borrow_roots_created();
        machine
            .start_owned_once("Entry", &mut args, 0, false)
            .unwrap();
        assert!(
            args.is_empty() && args.capacity() == 0,
            "the original input has actually transferred"
        );
        pause_loan_at(&mut machine, &mut host, |machine| {
            if root_value {
                matches!(top_work(machine), Some(Work::Eval(id)) if matches!(machine.code.expression(*id).kind, ExprCode::StringBorrow(_)))
            } else {
                matches!(top_work(machine), Some(Work::PrepareArguments { .. }))
            }
        });
        let caller = machine.calls.get(0).unwrap().env.clone();
        assert_binding_buffer(&caller, pointer, capacity, false);
        let staged_peak_before = machine.peak_argument_bytes;
        if root_value {
            assert!(machine.lease_capacity > 0 && machine.lease_bytes > 0);
            assert_eq!(machine.lease_root_bytes, 0);
            // No operand growth obscures the root-value admission boundary.
            machine.reserve_values(1, Span::default()).unwrap();
            machine.value_limits.max_storage_bytes =
                live_loan_bytes(&machine) + std::mem::size_of::<Value>() - 1;
        } else {
            assert_eq!(machine.lease_capacity, 0);
            machine.value_limits.max_storage_bytes =
                live_loan_bytes(&machine) + 2 * std::mem::size_of::<Value>();
        }
        eprintln!("String lease admission root_value={root_value} limit={} actual values={} arguments={} lease_capacity={} lease_bytes={} root_bytes={} input_payload={}",
            machine.value_limits.max_storage_bytes, machine.values.charged_storage_bytes(),
            machine.argument_bytes, machine.lease_capacity, machine.lease_bytes,
            machine.lease_root_bytes, machine.string_payload_bytes);
        let error = finish_loan(&mut machine, &mut host)
            .err()
            .expect("the real shared storage budget must reject");
        assert!(error
            .message
            .contains("internal execution storage rejected"));
        assert_eq!(machine.metrics.entered_calls, 1);
        assert_eq!(machine.metrics.lease_roots_created, 0);
        assert_eq!(crate::value::borrow_roots_created(), before_roots);
        if !root_value {
            assert!(
                machine.peak_argument_bytes > staged_peak_before,
                "the actual staged Vec admitted before lease backing rejected"
            );
            assert_eq!(machine.metrics.peak_lease_capacity, 0);
        } else {
            assert!(machine.metrics.peak_lease_capacity > 0);
        }
        assert_binding_buffer(&caller, pointer, capacity, false);
        assert!(host.caller_owners.is_empty());
        drained_leases(&machine);
    }
}

#[test]
fn unsupported_string_view_trees_keep_the_input_and_legacy_selector_without_effects() {
    for source in [
        "fn Copy(&text: str): str { return text.clone() }\nfn Entry(text: str): str { return Copy(text) }",
        "async fn Entry(text: str): int! { return ok(text.len()) }",
        "fn Number(): int { return 1 }\nfn Read(&text: str, n: int): int { return text.len() + n }\nfn Entry(text: str): int { return Read(text, Number()) }",
    ] {
        let mut host = production_interpreter(source);
        let entry = host.functions.get("Entry").unwrap().clone();
        assert!(matches!(CodeHandle::compile_borrowed_string_named(&host.functions, &entry, Limits::default()), Err(AdmissionError::Unsupported(_))));
        let (text, pointer, capacity) = text_input();
        let mut args = vec![Value::String(text)];
        let before_roots = crate::value::borrow_roots_created();
        assert!(try_execute_borrowed_string(&mut host, &entry, &mut args, 0).is_none());
        assert!(try_execute_owned(&mut host, &entry, &mut args, 0).is_none());
        assert_eq!(host.steps, 0);
        assert_eq!(host.call_depth, 0);
        assert_eq!(crate::value::borrow_roots_created(), before_roots);
        let Value::String(text) = &args[0] else { panic!("selection must retain the original owner"); };
        assert_eq!(text.as_ptr(), pointer);
        assert_eq!(text.capacity(), capacity);
        assert!(host.task_runtime.is_none(), "unsupported async selection must not start a child");
        if !entry.is_async {
            let actual = host.call_function("Entry", args, Span::default(), 0).unwrap();
            if source.contains("Copy") { assert_eq!(actual, Value::String("\u{4e2d}\u{e9}\0".into())); }
            else { assert_eq!(actual, Value::Int(4)); }
        }
    }
}

#[test]
fn projected_string_views_stay_legacy_and_illegal_view_escape_stays_checker_rejected() {
    let source = "fn Read(&text: str): int { return text.len() + text.byte_len() }";
    let mut host = production_interpreter(source);
    let entry = host.functions.get("Read").unwrap().clone();
    let (text, pointer, capacity) = text_input();
    let mut fields = HashMap::new();
    fields.insert("text".into(), Value::String(text));
    let mut env = Env::new();
    env.define_owned(
        "record".into(),
        Value::Object(fields),
        false,
        Span::default(),
    )
    .unwrap();
    let (view, lease) = env.borrow("record", Span::default()).unwrap();
    let Value::Borrowed(view) = view else {
        unreachable!()
    };
    let projected = view
        .project(
            crate::value::BorrowProjection::Field("text".into()),
            Span::default(),
        )
        .unwrap();
    let Value::Borrowed(projected_view) = &projected else {
        panic!("projection must remain a weak view");
    };
    assert!(!projected_view.is_root_view());
    let mut args = vec![projected];
    let before_roots = crate::value::borrow_roots_created();
    assert!(try_execute_borrowed_string(&mut host, &entry, &mut args, 0).is_none());
    assert!(try_execute_owned(&mut host, &entry, &mut args, 0).is_none());
    assert_eq!(host.steps, 0);
    assert_eq!(crate::value::borrow_roots_created(), before_roots);
    assert_eq!(
        host.call_function("Read", args, Span::default(), 0)
            .unwrap(),
        Value::Int(9)
    );
    drop(lease);
    env.with_value("record", Span::default(), |value| {
        let Value::Object(fields) = value else {
            panic!("the original external object owner must restore");
        };
        let Value::String(text) = &fields["text"] else {
            unreachable!()
        };
        assert_eq!(text.as_ptr(), pointer);
        assert_eq!(text.capacity(), capacity);
        Ok(())
    })
    .unwrap();

    for illegal in [
        "fn Escape(&text: str): str { return text }\nfn main() {}",
        "async fn Escape(&text: str): int! { return ok(1) }\nfn main() {}",
    ] {
        let program = Parser::new(Lexer::new(illegal).tokenize().unwrap())
            .parse_program()
            .unwrap();
        let error = crate::checker::Checker::new().check(&program).unwrap_err();
        assert!(error.message.contains("borrow"));
    }
}
