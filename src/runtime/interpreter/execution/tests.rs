use super::*;

const RECURSION: &str = r#"
fn Descend(n: int): int {
    if (n <= 0) { return 0 }
    return Descend(n - 1)
}
"#;

fn functions(source: &str) -> HashMap<String, FnDecl> {
    let source = format!("{source}\nfn main() {{}}\n");
    let program = Parser::new(Lexer::new(&source).tokenize().unwrap())
        .parse_program()
        .unwrap();
    crate::checker::Checker::new().check(&program).unwrap();
    program
        .items
        .into_iter()
        .map(|item| {
            let Item::Function(function) = item else {
                panic!("execution fixture must contain only top-level functions");
            };
            (function.name.clone(), function)
        })
        .collect()
}

fn assert_drained(metrics: &Metrics, initial_depth: usize) {
    assert_eq!(metrics.entered_calls, metrics.retired_calls, "{metrics:?}");
    assert_eq!(metrics.remaining_calls, 0, "{metrics:?}");
    assert_eq!(metrics.remaining_work, 0, "{metrics:?}");
    assert_eq!(metrics.remaining_values, 0, "{metrics:?}");
    assert_eq!(metrics.logical_depth, initial_depth, "{metrics:?}");
    assert!(
        metrics.retained_backing_bytes <= metrics.peak_backing_bytes_upper_bound,
        "{metrics:?}"
    );
}

fn assert_depth_error(result: KuResult<Value>) {
    let error = result.expect_err("the existing logical call limit must reject");
    assert_eq!(error.domain.as_deref(), Some("runtime"));
    assert_eq!(error.code.as_deref(), Some("call_depth_exceeded"));
}

#[test]
fn flat_named_recursion_accepts_exactly_512_active_calls() {
    assert_eq!(MAX_CALL_DEPTH, 512);
    let source = functions(RECURSION);
    let (result, metrics) = execute(
        &source,
        "Descend",
        &[Value::Int(511)],
        0,
        0,
        Limits::default(),
    );
    assert_eq!(result.unwrap(), Value::Int(0));
    assert_eq!(metrics.peak_calls, 512);
    assert_eq!(metrics.entered_calls, 512);
    assert_drained(&metrics, 0);
}

#[test]
fn flat_named_recursion_rejects_first_excess_call_and_original_reproducer() {
    let source = functions(RECURSION);
    for argument in [512, 1024] {
        let (result, metrics) = execute(
            &source,
            "Descend",
            &[Value::Int(argument)],
            0,
            0,
            Limits::default(),
        );
        assert_depth_error(result);
        assert_eq!(metrics.peak_calls, 512);
        assert_eq!(metrics.entered_calls, 512);
        assert_drained(&metrics, 0);
    }
}

#[test]
fn admission_preserves_passed_depth_and_existing_interpreter_call_count() {
    let source = functions(RECURSION);
    for (depth, initial_depth, entered) in [(511, 0, 1), (0, 511, 1), (512, 0, 0), (0, 512, 0)] {
        let (result, metrics) = execute(
            &source,
            "Descend",
            &[Value::Int(1)],
            depth,
            initial_depth,
            Limits::default(),
        );
        assert_depth_error(result);
        assert_eq!(metrics.entered_calls, entered);
        assert_drained(&metrics, initial_depth);
    }
}

#[test]
fn non_tail_recursion_resumes_each_pending_return_expression() {
    let source = functions(
        r#"
fn Sum(n: int): int {
    if (n <= 0) { return 0 }
    return n + Sum(n - 1)
}
"#,
    );
    let (result, metrics) = execute(&source, "Sum", &[Value::Int(500)], 0, 0, Limits::default());
    assert_eq!(result.unwrap(), Value::Int(125250));
    assert_eq!(metrics.entered_calls, 501);
    assert_eq!(metrics.peak_calls, 501);
    assert_drained(&metrics, 0);
}

#[test]
fn nested_arguments_keep_source_positions_and_evaluate_each_call_once() {
    let source = functions(
        r#"
fn Echo(value: int): int { return value }
fn Pack(a: int, b: int, c: int): int { return a * 10000 + b * 100 + c }
fn Entry(): int { return Pack(Echo(7), Echo(11), Echo(13)) }
"#,
    );
    let (result, metrics) = execute(&source, "Entry", &[], 0, 0, Limits::default());
    assert_eq!(result.unwrap(), Value::Int(71113));
    assert_eq!(metrics.entered_calls, 5);
    assert_eq!(metrics.peak_calls, 2);
    assert_drained(&metrics, 0);
}

#[test]
fn argument_error_stops_later_arguments_and_target_entry_in_source_order() {
    let source = functions(
        r#"
fn First(): int { return 1 / 0 }
fn Second(): int { return 9223372036854775807 + 1 }
fn Third(n: int): int { return Third(n + 1) }
fn Target(a: int, b: int, c: int): int { return a + b + c }
fn Entry(): int { return Target(First(), Second(), Third(0)) }
"#,
    );
    let (result, metrics) = execute(&source, "Entry", &[], 0, 0, Limits::default());
    assert_eq!(result.unwrap_err().message, "division by zero");
    // Only Entry and the first argument's call entered; overflow, recursion
    // and the target body must not replace the first observable failure.
    assert_eq!(metrics.entered_calls, 2);
    assert_eq!(metrics.peak_calls, 2);
    assert_drained(&metrics, 0);
}

#[test]
fn boolean_short_circuit_does_not_enter_rejected_right_hand_calls() {
    let source = functions(
        r#"
fn Broken(): bool { return 1 / 0 == 0 }
fn Entry(): bool { return (false && Broken()) || (true || Broken()) }
"#,
    );
    let (result, metrics) = execute(&source, "Entry", &[], 0, 0, Limits::default());
    assert_eq!(result.unwrap(), Value::Bool(true));
    assert_eq!(metrics.entered_calls, 1);
    assert_drained(&metrics, 0);
}

#[test]
fn block_scope_assignment_return_and_fallthrough_keep_existing_semantics() {
    for (body, expected) in [
        (
            "value = 7\nif (true) { value = 11 }\nreturn value",
            Value::Int(11),
        ),
        ("if (true) { return 23 }\nreturn 1 / 0", Value::Int(23)),
        ("if (false) { return 1 / 0 }", Value::Null),
    ] {
        let source = functions(&format!("fn Entry() {{ {body} }}"));
        let (result, metrics) = execute(&source, "Entry", &[], 0, 0, Limits::default());
        assert_eq!(result.unwrap(), expected);
        assert_eq!(metrics.entered_calls, 1);
        assert_drained(&metrics, 0);
    }
}

#[test]
fn nested_runtime_error_retires_all_entered_frames() {
    let source = functions(
        r#"
fn Descend(n: int): int {
    if (n <= 0) { return 9223372036854775807 + 1 }
    return Descend(n - 1)
}
"#,
    );
    let (result, metrics) = execute(
        &source,
        "Descend",
        &[Value::Int(63)],
        0,
        0,
        Limits::default(),
    );
    assert_eq!(result.unwrap_err().message, "integer overflow");
    assert_eq!(metrics.entered_calls, 64);
    assert_drained(&metrics, 0);
}

#[test]
fn independent_count_budgets_reject_and_roll_back_execution_state() {
    let source = functions(
        r#"
fn Descend(n: int): int {
    if (n <= 0) { return 0 }
    return Descend(n - 1)
}
fn Entry(): int { return 10 + 20 }
"#,
    );
    let mut calls = Limits::default();
    calls.calls.max_frames = 3;
    let mut work = Limits::default();
    work.work.max_frames = 2;
    let mut values = Limits::default();
    values.values.max_frames = 1;
    for (limits, entry, args, entered) in [
        (calls, "Descend", vec![Value::Int(10)], 3),
        (work, "Entry", vec![], 1),
        (values, "Entry", vec![], 1),
    ] {
        let (result, metrics) = execute(&source, entry, &args, 0, 0, limits);
        assert_eq!(
            result.unwrap_err().message,
            "internal execution storage rejected: FrameLimit"
        );
        assert_eq!(metrics.entered_calls, entered);
        assert_drained(&metrics, 0);
    }
}

#[test]
fn independent_byte_budgets_reject_before_call_admission() {
    let source = functions("fn Entry(): int { return 7 }");
    let mut calls = Limits::default();
    calls.calls.max_storage_bytes = 0;
    let mut work = Limits::default();
    work.work.max_storage_bytes = 0;
    let mut values = Limits::default();
    values.values.max_storage_bytes = 0;
    for limits in [calls, work, values] {
        let (result, metrics) = execute(&source, "Entry", &[], 0, 0, limits);
        assert_eq!(
            result.unwrap_err().message,
            "internal execution storage rejected: StorageLimit"
        );
        assert_eq!(metrics.entered_calls, 0);
        assert_drained(&metrics, 0);
    }
}

#[test]
fn preflight_budgets_reject_without_starting_the_entry_function() {
    let source = functions("fn Entry(): int { return 7 }");
    let mut count = Limits::default();
    count.syntax.max_frames = 0;
    let mut bytes = Limits::default();
    bytes.syntax.max_storage_bytes = 0;
    let fuel = Limits {
        syntax_steps: 0,
        ..Limits::default()
    };
    for (limits, message) in [
        (count, "internal execution storage rejected: FrameLimit"),
        (bytes, "internal execution storage rejected: StorageLimit"),
        (
            fuel,
            "internal execution prototype does not support this source",
        ),
    ] {
        let (result, metrics) = execute(&source, "Entry", &[], 0, 7, limits);
        assert_eq!(result.unwrap_err().message, message);
        assert_eq!(metrics.entered_calls, 0);
        assert_eq!(metrics.dispatches, 0);
        assert_eq!(metrics.retained_backing_bytes, 0);
        assert_drained(&metrics, 7);
    }
}

#[test]
fn prototype_dispatch_fuel_exhaustion_rolls_back_live_calls() {
    let source = functions(RECURSION);
    let limits = Limits {
        prototype_dispatch_steps: 32,
        ..Limits::default()
    };
    let (result, metrics) = execute(&source, "Descend", &[Value::Int(8)], 0, 0, limits);
    assert_eq!(
        result.unwrap_err().message,
        "internal execution prototype test fuel exhausted"
    );
    assert_eq!(metrics.dispatches, 32);
    assert!(metrics.entered_calls > 0);
    assert_drained(&metrics, 0);
}

#[test]
fn unsupported_unreachable_functions_are_rejected_before_any_execution() {
    for unsupported in [
        "fn Unused(): str { return \"owned\" }",
        "fn Unused(&value: int): int { return value }",
        "async fn Unused(): int! { return ok(1) }",
        "fn Unused() { try { marker = 1 } finally { marker = 2 } }",
    ] {
        let source = functions(&format!("fn Entry(): int {{ return 7 }}\n{unsupported}"));
        let (result, metrics) = execute(&source, "Entry", &[], 0, 0, Limits::default());
        assert_eq!(
            result.unwrap_err().message,
            "internal execution prototype does not support this source"
        );
        assert_eq!(metrics.entered_calls, 0);
        assert_eq!(metrics.dispatches, 0);
        assert_drained(&metrics, 0);
    }
}

#[test]
fn argument_count_rejection_drains_staged_values_without_entering_a_frame() {
    let source = functions("fn Entry(a: int, b: int): int { return a + b }");
    let (result, metrics) = execute(&source, "Entry", &[Value::Int(7)], 0, 0, Limits::default());
    assert_eq!(
        result.unwrap_err().message,
        "function 'Entry' expects 2 arguments but got 1"
    );
    assert_eq!(metrics.entered_calls, 0);
    assert_drained(&metrics, 0);
}

#[test]
fn failed_execution_can_reuse_its_drained_machine_for_a_healthy_call() {
    let source = functions(
        r#"
fn Entry(n: int): int {
    if (n == 1) { return 1 / 0 }
    return n + 1
}
"#,
    );
    let mut machine = Machine::new(&source, Limits::default(), 0);
    assert_eq!(
        machine
            .run("Entry", &[Value::Int(1)], 0)
            .unwrap_err()
            .message,
        "division by zero"
    );
    assert_drained(&machine.metrics, 0);
    assert_eq!(
        machine.run("Entry", &[Value::Int(7)], 0).unwrap(),
        Value::Int(8)
    );
    assert_eq!(machine.metrics.entered_calls, 2);
    assert_drained(&machine.metrics, 0);
}

fn production_interpreter(source: &str) -> Interpreter {
    let mut interpreter = Interpreter::new();
    interpreter.functions.extend(functions(source));
    interpreter
}

#[test]
fn production_named_calls_preserve_existing_depth_and_restore_it_on_rejection() {
    let mut interpreter = production_interpreter(RECURSION);
    interpreter.call_depth = 7;
    assert_eq!(
        interpreter
            .call_function("Descend", vec![Value::Int(504)], Span::default(), 0)
            .unwrap(),
        Value::Int(0)
    );
    assert_eq!(interpreter.call_depth, 7);
    assert!(interpreter.steps > 0, "production execution must poll tick");
    assert_depth_error(interpreter.call_function(
        "Descend",
        vec![Value::Int(505)],
        Span::default(),
        0,
    ));
    assert_eq!(interpreter.call_depth, 7);
    assert_depth_error(interpreter.call_function(
        "Descend",
        vec![Value::Int(1)],
        Span::default(),
        511,
    ));
    assert_eq!(interpreter.call_depth, 7);
    assert_eq!(
        interpreter
            .call_function("Descend", vec![Value::Int(0)], Span::default(), 0)
            .unwrap(),
        Value::Int(0)
    );
    assert_eq!(interpreter.call_depth, 7);
}

#[test]
fn production_scalar_tree_ignores_unrelated_owned_and_async_functions() {
    let mut interpreter = production_interpreter(&format!(
        "{RECURSION}\nfn Owned(): str {{ return \"kept\" }}\nasync fn Unused(): int! {{ return ok(1) }}"
    ));
    let entry = interpreter.functions.get("Descend").unwrap();
    assert!(preflight(&interpreter.functions, Limits::default()).is_err());
    preflight_tree(&interpreter.functions, Limits::default(), Some(entry)).unwrap();
    assert_eq!(
        interpreter
            .call_function("Descend", vec![Value::Int(32)], Span::default(), 0)
            .unwrap(),
        Value::Int(0)
    );
    assert_eq!(
        interpreter
            .call_function("Owned", vec![], Span::default(), 0)
            .unwrap(),
        Value::String("kept".into())
    );
    assert_eq!(interpreter.call_depth, 0);
}

#[test]
fn production_bare_global_function_values_keep_the_existing_callable_behavior() {
    let mut interpreter = production_interpreter(&format!(
        "{RECURSION}\nfn ReturnFunction() {{ return Descend }}\nfn Entry(): int {{ alias = Descend\nreturn alias(3) }}"
    ));
    for name in ["ReturnFunction", "Entry"] {
        let entry = interpreter.functions.get(name).unwrap().clone();
        assert!(preflight_tree(&interpreter.functions, Limits::default(), Some(&entry)).is_err());
        assert!(try_execute(&mut interpreter, &entry, &[], 0).is_none());
    }
    assert_eq!(interpreter.steps, 0, "selection must have no body effects");
    let value = interpreter
        .call_function("ReturnFunction", vec![], Span::default(), 0)
        .unwrap();
    let Value::Function {
        params,
        param_modes,
        is_async,
        ..
    } = value
    else {
        panic!("a bare global function must remain a first-class function value");
    };
    assert_eq!(params, ["n"]);
    assert_eq!(param_modes, [ParamMode::Owned]);
    assert!(!is_async);
    assert_eq!(
        interpreter
            .call_function("Entry", vec![], Span::default(), 0)
            .unwrap(),
        Value::Int(0)
    );
    assert_eq!(interpreter.call_depth, 0);
}

#[test]
fn production_unmigrated_loop_keeps_its_existing_legal_behavior() {
    let mut interpreter = production_interpreter(
        r#"
fn Entry(): int {
    value = 0
    while (value < 3) { value = value + 1 }
    return value
}
"#,
    );
    let entry = interpreter.functions.get("Entry").unwrap().clone();
    assert!(try_execute(&mut interpreter, &entry, &[], 0).is_none());
    assert_eq!(interpreter.steps, 0);
    assert_eq!(
        interpreter
            .call_function("Entry", vec![], Span::default(), 0)
            .unwrap(),
        Value::Int(3)
    );
    assert_eq!(interpreter.call_depth, 0);
}

#[test]
fn production_poll_propagates_cancellation_and_restores_existing_call_depth() {
    let previous = crate::runtime::task::current_execution_termination();
    {
        let _execution = ExecutionTerminationGuard::enter();
        let context = RuntimeTermination {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: Instant::now() + Duration::from_secs(1),
        };
        let mut interpreter = production_interpreter(RECURSION);
        interpreter.call_depth = 9;
        interpreter.termination = Some(context);
        let error = interpreter
            .call_function("Descend", vec![Value::Int(32)], Span::default(), 0)
            .expect_err("the scalar production driver must execute cancellation safepoints");
        assert_eq!(error.runtime_termination(), Some(context));
        assert_eq!(interpreter.call_depth, 9);
        assert_eq!(interpreter.steps, 0);
        assert!(interpreter.pending_fail.is_none());
        assert!(interpreter.caller_owners.is_empty());
    }
    assert_eq!(
        crate::runtime::task::current_execution_termination(),
        previous
    );
    let mut healthy = production_interpreter(RECURSION);
    assert_eq!(
        healthy
            .call_function("Descend", vec![Value::Int(1)], Span::default(), 0)
            .unwrap(),
        Value::Int(0)
    );
    assert_eq!(healthy.call_depth, 0);
}

#[test]
fn production_execution_does_not_apply_the_prototype_dispatch_fuel_limit() {
    // A small, checker-valid static tree performs enough resumed calls to
    // exceed test fuel without increasing simultaneous logical call depth.
    let source = r#"
fn Count(n: int): int {
    if (n <= 0) { return 1 }
    return Count(n - 1) + Count(n - 1)
}
"#;
    let functions = functions(source);
    let (prototype_result, metrics) = execute(
        &functions,
        "Count",
        &[Value::Int(16)],
        0,
        0,
        Limits::default(),
    );
    assert_eq!(
        prototype_result.unwrap_err().message,
        "internal execution prototype test fuel exhausted"
    );
    assert_drained(&metrics, 0);
    let mut interpreter = production_interpreter(source);
    assert_eq!(
        interpreter
            .call_function("Count", vec![Value::Int(16)], Span::default(), 0)
            .unwrap(),
        Value::Int(65536)
    );
    assert!(interpreter.steps > 0);
    assert_eq!(interpreter.call_depth, 0);
}
