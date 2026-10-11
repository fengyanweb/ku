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
    // Entry now reserves CallExit, its real body ScopeExit and the Block.
    // Admit that first call, then reject its next continuation growth.
    work.work.max_frames = 3;
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
fn production_scalar_loop_selects_flat_execution_and_keeps_its_legal_behavior() {
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
    assert_eq!(
        try_execute(&mut interpreter, &entry, &[], 0)
            .expect("the complete scalar while tree must select the flat driver")
            .unwrap()
            .into_call_value(Span::default(), "function")
            .unwrap(),
        Value::Int(3)
    );
    assert_eq!(
        interpreter
            .call_function("Entry", vec![], Span::default(), 0)
            .unwrap(),
        Value::Int(3)
    );
    assert_eq!(interpreter.call_depth, 0);
}

#[test]
fn production_recursive_while_and_for_preserve_512_call_boundary_on_real_workers() {
    let (_, test_name) = concat!(
        module_path!(),
        "::production_recursive_while_and_for_preserve_512_call_boundary_on_real_workers"
    )
    .split_once("::")
    .expect("worker regression must have a module-qualified test name");
    const CHILD: &str = "KU_EXECUTION_LOOP_STACK_SAFETY_CHILD";
    let completed = format!("loop worker regression completed: {test_name}");
    if std::env::var(CHILD).as_deref() != Ok(test_name) {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", test_name, "--nocapture"])
            .env(CHILD, test_name);
        let output = finally_bounded_process::run_bounded(
            &mut command,
            Duration::from_secs(15),
            finally_bounded_process::OutputLimits::new(64 * 1024, 128 * 1024),
        )
        .expect("recursive loop regression must not hang its supervisor");
        assert!(
            output.status.success(),
            "worker child must exit normally: {:?}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .any(|line| line == completed),
            "zero selected tests or an incomplete matrix cannot pass\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    assert_eq!(MAX_CALL_DEPTH, 512);
    for source in [
        r#"
fn Descend(n: int): int {
    while (n > 0) { return Descend(n - 1) }
    return 0
}
"#,
        r#"
fn Descend(n: int): int {
    if (n <= 0) { return 0 }
    for index in 1 { return Descend(n - 1) }
    return 0
}
"#,
    ] {
        for argument in [511, 512] {
            let runtime = TaskRuntime::new();
            let mut interpreter = production_interpreter(source);
            interpreter.task_runtime = Some(runtime.clone());
            interpreter.async_execution = true;
            let parent_id = thread::current().id();
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let (result_tx, result_rx) = mpsc::sync_channel(1);
            let task = runtime.spawn(move || {
                let worker = thread::current();
                let _ = ready_tx.send((worker.id(), worker.name().unwrap_or("").to_owned()));
                let result = interpreter.call_function(
                    "Descend",
                    vec![Value::Int(argument)],
                    Span::default(),
                    0,
                );
                let _ = result_tx.send((result, interpreter.call_depth));
                Ok(Value::Null)
            });
            // Channel waits keep the caller from helping this job on its own
            // stack. Abort remains a failing bounded child-process outcome.
            let ready = ready_rx.recv_timeout(Duration::from_secs(3));
            let outcome = result_rx.recv_timeout(Duration::from_secs(3));
            let admission_error = task.as_ref().err().cloned();
            drop(task);
            let drained = runtime.cancel_all_and_wait(Duration::from_secs(2));
            let snapshot = runtime.snapshot();
            drained.unwrap();
            let snapshot = snapshot.unwrap();
            assert!(admission_error.is_none(), "{admission_error:?}");
            let (worker_id, worker_name) = ready.expect("a real worker must report entry");
            assert_ne!(worker_id, parent_id);
            assert!(worker_name.starts_with("ku-task-"), "{worker_name}");
            let (result, depth) = outcome.expect("the real worker must finish within the bound");
            if argument == 511 {
                assert_eq!(result.unwrap(), Value::Int(0));
            } else {
                assert_depth_error(result);
            }
            assert_eq!(depth, 0);
            assert_eq!(snapshot.active_tasks, 0);
            assert_eq!(snapshot.queued_tasks, 0);
            assert_eq!(snapshot.queued_blocking_jobs, 0);
            assert_eq!(snapshot.running_blocking_jobs, 0);
        }
    }
    eprintln!("{completed}");
}

#[test]
fn production_nested_loop_controls_iterator_assignment_and_return_keep_scope_semantics() {
    for (source, expected) in [
        (
            r#"
fn Entry(): int {
    total = 0
    for outer in 3 {
        for inner in 5 {
            if (inner == 1) { continue }
            if (inner == 3) { break }
            inner = inner + 10
            total = total + outer * 10 + inner
        }
        current = 0
        while (current < 3) {
            current = current + 1
            if (current == 1) { continue }
            total = total + 100
            break
        }
    }
    return total
}
"#,
            426,
        ),
        (
            r#"
fn Entry(): int {
    for outer in 3 { while (true) { return outer + 17 } }
    return 0
}
"#,
            17,
        ),
    ] {
        let mut interpreter = production_interpreter(source);
        let entry = interpreter.functions.get("Entry").unwrap().clone();
        assert_eq!(
            try_execute(&mut interpreter, &entry, &[], 0)
                .expect("all nested loops must belong to the same selected tree")
                .unwrap()
                .into_call_value(Span::default(), "function")
                .unwrap(),
            Value::Int(expected)
        );
        assert_eq!(
            interpreter
                .call_function("Entry", vec![], Span::default(), 0)
                .unwrap(),
            Value::Int(expected)
        );
        assert_eq!(interpreter.call_depth, 0);
    }
}

#[test]
fn production_loop_results_and_ticks_match_the_same_ast_in_the_original_engine() {
    for loop_kind in ["while", "for"] {
        for (exit, argument, expected, message) in [
            ("", Value::Int(0), Some(Value::Int(0)), None),
            ("", Value::Int(1), Some(Value::Int(1)), None),
            ("continue", Value::Int(3), Some(Value::Int(3)), None),
            ("break", Value::Int(3), Some(Value::Int(1)), None),
            (
                "return value + 100",
                Value::Int(3),
                Some(Value::Int(101)),
                None,
            ),
            (
                "return 1 / 0",
                Value::Int(1),
                None,
                Some("division by zero"),
            ),
        ] {
            let loop_source = if loop_kind == "while" {
                format!(
                    "while (current < n) {{ current = current + 1\nvalue = Leaf(value)\n{exit} }}"
                )
            } else {
                format!("for index in n {{ value = Leaf(value)\n{exit} }}")
            };
            let source = format!(
                "fn Leaf(n: int): int {{ return n + 1 }}\nfn Entry(n: int): int {{\nvalue = 0\ncurrent = 0\n{loop_source}\nreturn value\n}}"
            );
            let mut selected = production_interpreter(&source);
            let entry = selected.functions.get("Entry").unwrap().clone();
            let result = try_execute(&mut selected, &entry, std::slice::from_ref(&argument), 0)
                .expect("the complete scalar loop tree must be admitted")
                .and_then(|flow| flow.into_call_value(Span::default(), "function"));
            let mut original = production_interpreter(&source);
            // Direct entry runs this identical top-level loop AST in the old
            // evaluator. Its Leaf calls do not recurse or contain any loops.
            let old_result = original
                .call_function_direct_flow(entry, vec![argument], Span::default(), 0)
                .and_then(|flow| flow.into_call_value(Span::default(), "function"));
            assert_eq!(result, old_result, "{loop_kind}: {exit}");
            if let Some(expected) = expected {
                assert_eq!(result.unwrap(), expected, "{loop_kind}: {exit}");
            } else {
                assert_eq!(result.unwrap_err().message, message.unwrap());
            }
            assert_eq!(selected.steps, original.steps, "{loop_kind}: {exit}");
            assert_eq!(selected.call_depth, 0);
            assert_eq!(original.call_depth, 0);
        }
    }

    // A checker-valid int parameter can still receive a scalar of another
    // type at this runtime API boundary; retain the existing runtime errors.
    let source = "fn Entry(n: int): int { for index in n {} return 0 }";
    for (argument, message) in [
        (
            Value::Int(-1),
            "for int iterator expects a non-negative int".to_owned(),
        ),
        (
            Value::Bool(true),
            "type error: for expects array or int but got bool".to_owned(),
        ),
        (
            Value::Float(1.0),
            "type error: for expects array or int but got float".to_owned(),
        ),
        (
            Value::Null,
            "type error: for expects array or int but got null".to_owned(),
        ),
    ] {
        let mut selected = production_interpreter(source);
        let entry = selected.functions.get("Entry").unwrap().clone();
        let error = try_execute(&mut selected, &entry, std::slice::from_ref(&argument), 0)
            .expect("the scalar iterable must use runtime type arbitration")
            .err()
            .expect("invalid scalar iterable must reject");
        let mut original = production_interpreter(source);
        let old_error = original
            .call_function_direct_flow(entry, vec![argument], Span::default(), 0)
            .err()
            .expect("the original runtime must reject the same iterable");
        assert_eq!(error, old_error);
        assert_eq!(error.message, message);
        assert_eq!(selected.steps, original.steps);
        assert_eq!(selected.call_depth, 0);
        assert_eq!(original.call_depth, 0);
    }
}

#[test]
fn production_owned_iterable_and_for_self_shadow_remain_legal_in_the_original_engine() {
    let mut interpreter = production_interpreter(
        r#"
fn Entry(): int {
    total = 0
    for item in [1, 2, 3] { total = total + item }
    return total
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
        Value::Int(6)
    );

    let mut interpreter = production_interpreter(
        r#"
fn Make() {
    fn local(n: int): int {
        total = 0
        for local in n { total = total + 1 }
        return total
    }
    return local.clone()
}
"#,
    );
    let value = interpreter
        .call_function("Make", vec![], Span::default(), 0)
        .unwrap();
    assert!(
        try_execute_function_value(&mut interpreter, &function_value_call(&value, 3, 0)).is_none()
    );
    assert_eq!(
        interpreter
            .call_function_value(function_value_call(&value, 3, 0))
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

fn function_value_call(value: &Value, argument: i64, depth: usize) -> FunctionValueCall<'_> {
    let Value::Function {
        params,
        param_modes,
        body,
        captures,
        self_name,
        is_async,
    } = value
    else {
        panic!("checked factory must return a real function value");
    };
    FunctionValueCall {
        params,
        param_modes,
        body,
        captures,
        self_name,
        is_async: *is_async,
        args: vec![Value::Int(argument)],
        span: Span::default(),
        depth,
    }
}

#[test]
fn production_local_non_tail_self_calls_read_stable_scalar_captures() {
    let mut interpreter = production_interpreter(
        r#"
fn Make() {
    OFFSET = 20
    fn local(n: int): int {
        if (n <= 0) { return OFFSET }
        return n + local(n - 1)
    }
    return local.clone()
}
"#,
    );
    let value = interpreter
        .call_function("Make", vec![], Span::default(), 0)
        .unwrap();
    assert_eq!(
        try_execute_function_value(&mut interpreter, &function_value_call(&value, 3, 0))
            .expect("stable read-only scalar captures should select the flat driver")
            .unwrap()
            .into_call_value(Span::default(), "function value")
            .unwrap(),
        Value::Int(26)
    );
    assert_eq!(
        interpreter
            .call_function_value(function_value_call(&value, 3, 0))
            .unwrap(),
        Value::Int(26)
    );
    assert_eq!(interpreter.call_depth, 0);
}

#[test]
fn production_shared_capture_writes_use_flat_epilogues_and_foreign_mutable_reads_keep_original_engine(
) {
    let runtime = TaskRuntime::new();
    let task = runtime
        .spawn(|| {
            let mut interpreter = production_interpreter(
                r#"
fn Make() {
    captured = 10
    fn local(n: int): int {
        captured = captured + 1
        if (n <= 0) { return captured }
        return n + local(n - 1)
    }
    captured = 20
    return local.clone()
}
"#,
            );
            let value = interpreter.call_function("Make", vec![], Span::default(), 0)?;
            let call = function_value_call(&value, 3, 0);
            assert_ne!(crate::runtime::task::current_task_id(), 0);
            assert!(call.captures.is_stable_scalar_binding("captured"));
            assert_eq!(
                call.captures.get("captured", Span::default())?,
                Value::Int(20)
            );
            assert_eq!(
                try_execute_function_value(&mut interpreter, &call)
                    .expect("same-Task mutable scalar captures should select the flat driver")?
                    .into_call_value(Span::default(), "function value")?,
                Value::Int(30)
            );
            assert_eq!(
                call.captures.get("captured", Span::default())?,
                Value::Int(24)
            );
            // Restore the actual shared cell so the independent public-entry
            // assertion starts from the same state, rather than adding twice.
            let mut shared = call.captures.clone();
            shared.assign_owned("captured", Value::Int(20), Span::default())?;
            assert_eq!(
                call.captures.get("captured", Span::default())?,
                Value::Int(20)
            );
            let result = interpreter.call_function_value(function_value_call(&value, 3, 0))?;
            assert_eq!(interpreter.call_depth, 0);
            let updated = function_value_call(&value, 0, 0)
                .captures
                .get("captured", Span::default())?;
            Ok(Value::Array(vec![result, updated]))
        })
        .unwrap();
    // Drain before inspecting the semantic result, including ordinary failures.
    let outcome = task.await_result();
    drop(task);
    let drained = runtime.cancel_all_and_wait(Duration::from_secs(2));
    let snapshot = runtime.snapshot().unwrap();
    drained.unwrap();
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(
        outcome.unwrap(),
        Value::Array(vec![Value::Int(30), Value::Int(24)])
    );

    let mut interpreter = production_interpreter(
        r#"
fn Make() {
    captured = 10
    fn local(n: int): int { return captured + n }
    return local.clone()
}
"#,
    );
    let mut value = interpreter
        .call_function("Make", vec![], Span::default(), 0)
        .unwrap();
    let (mut owner, foreign_capture) = thread::spawn(|| {
        let mut owner = Env::new();
        owner
            .define_owned("captured".into(), Value::Int(10), true, Span::default())
            .unwrap();
        let capture = owner.capture(&HashSet::from(["captured".into()]));
        (owner, capture)
    })
    .join()
    .unwrap();
    let Value::Function { captures, .. } = &mut value else {
        panic!("checked factory must return a function");
    };
    *captures = foreign_capture;
    let call = function_value_call(&value, 5, 0);
    assert_eq!(
        call.captures.get("captured", Span::default()).unwrap(),
        Value::Int(10)
    );
    assert!(!call.captures.is_stable_scalar_binding("captured"));
    assert!(try_execute_function_value(&mut interpreter, &call).is_none());
    assert_eq!(
        interpreter
            .call_function_value(function_value_call(&value, 5, 0))
            .unwrap(),
        Value::Int(15)
    );
    owner
        .assign_owned("captured", Value::Int(30), Span::default())
        .unwrap();
    assert!(
        try_execute_function_value(&mut interpreter, &function_value_call(&value, 5, 0)).is_none()
    );
    assert_eq!(
        interpreter
            .call_function_value(function_value_call(&value, 5, 0))
            .unwrap(),
        Value::Int(35)
    );
    assert_eq!(interpreter.call_depth, 0);
}

#[test]
fn production_expired_child_scope_stops_parent_capture_bind_without_resetting_deadline() {
    let runtime = TaskRuntime::new();
    let task = runtime
        .spawn(|| {
            assert_ne!(crate::runtime::task::current_task_id(), 0);
            let original = RuntimeTermination {
                reason: TerminationReason::Cancelled,
                cleanup_deadline: Instant::now()
                    .checked_sub(Duration::from_millis(1))
                    .expect("test clock must support a past instant"),
            };
            let assert_expired = |error: KuError| {
                assert_eq!(error.domain.as_deref(), Some("task"));
                assert_eq!(error.code.as_deref(), Some("shutdown_timeout"));
                assert_eq!(error.scope_cleanup(), Some(original));
                assert!(error.runtime_termination().is_none());
            };
            for (selected, return_expression) in [
                (true, "payload"),
                // The unselected complete tree has the same observable return:
                // this Owned expression is never evaluated after true payload.
                (false, "payload || (\"owned\" == \"owned\")"),
            ] {
                let mut interpreter = production_interpreter(&format!(
                    r#"
fn Make() {{
    captured = true
    fn local(active: bool): bool {{
        payload = true
        captured = active && local(false)
        return {return_expression}
    }}
    return local.clone()
}}
"#,
                ));
                let value = interpreter.call_function("Make", vec![], Span::default(), 0)?;
                let mut call = function_value_call(&value, 0, 0);
                call.args = vec![Value::Bool(true)];
                assert!(call.captures.is_stable_scalar_binding("captured"));
                assert_eq!(
                    call.captures.get("captured", Span::default())?,
                    Value::Bool(true)
                );
                {
                    let _execution = ExecutionTerminationGuard::enter();
                    let _cleanup = CleanupGuard::enter(original);
                    let result = try_execute_function_value(&mut interpreter, &call);
                    if selected {
                        let error = match result.expect("real scalar captures should be admitted") {
                            Err(error) => error,
                            Ok(_) => panic!("child scope expiry must stop the admitted tree"),
                        };
                        assert_expired(error);
                        // Child(false) really binds false. Its body scope contains
                        // payload and fails before parent can bind child(true).
                        assert_eq!(
                            call.captures.get("captured", Span::default())?,
                            Value::Bool(false)
                        );
                    } else {
                        assert!(result.is_none());
                        assert_eq!(
                            call.captures.get("captured", Span::default())?,
                            Value::Bool(true)
                        );
                    }
                }
                assert_eq!(interpreter.call_depth, 0);
                assert!(interpreter.termination.is_none());
                let mut shared = call.captures.clone();
                shared.assign_owned("captured", Value::Bool(true), Span::default())?;
                {
                    // Public routing must preserve exactly the same already
                    // expired D for both the selected tree and legacy contrast.
                    let _execution = ExecutionTerminationGuard::enter();
                    let _cleanup = CleanupGuard::enter(original);
                    assert_expired(
                        interpreter
                            .call_function_value(call)
                            .expect_err("child scope expiry must stop the public function call"),
                    );
                    assert_eq!(shared.get("captured", Span::default())?, Value::Bool(false));
                }
                assert_eq!(interpreter.call_depth, 0);
                assert!(interpreter.termination.is_none());
            }
            Ok(Value::Null)
        })
        .unwrap();
    let outcome = task.await_result();
    drop(task);
    let drained = runtime.cancel_all_and_wait(Duration::from_secs(2));
    let snapshot = runtime.snapshot().unwrap();
    drained.unwrap();
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(outcome.unwrap(), Value::Null);
}

#[test]
fn production_expired_loop_controls_stop_following_capture_writes_at_original_deadline() {
    let runtime = TaskRuntime::new();
    let task = runtime
        .spawn(|| {
            assert_ne!(crate::runtime::task::current_task_id(), 0);
            let original = RuntimeTermination {
                reason: TerminationReason::Cancelled,
                cleanup_deadline: Instant::now()
                    .checked_sub(Duration::from_millis(1))
                    .expect("test clock must support a past instant"),
            };
            let assert_expired = |error: KuError| {
                assert_eq!(error.domain.as_deref(), Some("task"));
                assert_eq!(error.code.as_deref(), Some("shutdown_timeout"));
                assert_eq!(error.scope_cleanup(), Some(original));
                assert!(error.runtime_termination().is_none());
            };
            for loop_kind in ["while", "for"] {
                for control in ["break", "continue"] {
                    let body =
                        format!("current = current + 1\npayload = true\ncaptured = 1\n{control}");
                    let loop_source = if loop_kind == "while" {
                        // Even a broken scope arbiter cannot turn this fixture
                        // into an infinite continue loop: current advances first.
                        format!("while (current < n) {{ {body} }}")
                    } else {
                        format!("for index in n {{ {body} }}")
                    };
                    let source = format!(
                        r#"
fn Make() {{
    captured = 0
    fn local(n: int): int {{
        current = 0
        {loop_source}
        captured = 2
        return captured
    }}
    return local.clone()
}}
"#,
                    );
                    let mut interpreter = production_interpreter(&source);
                    let value = interpreter.call_function("Make", vec![], Span::default(), 0)?;
                    let call = function_value_call(&value, 1, 0);
                    assert!(call.captures.is_stable_scalar_binding("captured"));
                    {
                        let _execution = ExecutionTerminationGuard::enter();
                        let _cleanup = CleanupGuard::enter(original);
                        let error = match try_execute_function_value(&mut interpreter, &call)
                            .expect("the complete scalar capture-writing loop must be admitted")
                        {
                            Err(error) => error,
                            Ok(_) => {
                                panic!("iteration scope expiry must reject {loop_kind} {control}")
                            }
                        };
                        assert_expired(error);
                        // The nonempty iteration's capture effect occurred,
                        // but its scope failure precedes loop control handling
                        // and therefore prevents the later captured = 2.
                        assert_eq!(
                            call.captures.get("captured", Span::default())?,
                            Value::Int(1)
                        );
                    }
                    assert_eq!(interpreter.call_depth, 0);
                    assert!(interpreter.termination.is_none());
                    let mut shared = call.captures.clone();
                    shared.assign_owned("captured", Value::Int(0), Span::default())?;
                    {
                        let _execution = ExecutionTerminationGuard::enter();
                        let _cleanup = CleanupGuard::enter(original);
                        assert_expired(
                            interpreter
                                .call_function_value(call)
                                .expect_err("public loop call must retain the expired scope error"),
                        );
                        assert_eq!(shared.get("captured", Span::default())?, Value::Int(1));
                    }
                    assert_eq!(interpreter.call_depth, 0);
                    assert!(interpreter.termination.is_none());
                }
            }
            Ok(Value::Null)
        })
        .unwrap();
    let outcome = task.await_result();
    drop(task);
    let drained = runtime.cancel_all_and_wait(Duration::from_secs(2));
    let snapshot = runtime.snapshot().unwrap();
    drained.unwrap();
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(outcome.unwrap(), Value::Null);
}

#[test]
fn production_self_callee_ticks_and_existing_512_call_budget_are_preserved() {
    let mut named = production_interpreter(RECURSION);
    assert_eq!(
        named
            .call_function("Descend", vec![Value::Int(3)], Span::default(), 0)
            .unwrap(),
        Value::Int(0)
    );
    let mut local = production_interpreter(&format!(
        "fn Make() {{ {RECURSION}\nreturn Descend.clone() }}"
    ));
    let value = local
        .call_function("Make", vec![], Span::default(), 0)
        .unwrap();
    local.steps = 0;
    assert_eq!(
        local
            .call_function_value(function_value_call(&value, 3, 0))
            .unwrap(),
        Value::Int(0)
    );
    // Three recursive calls each evaluate their function-value callee once.
    assert_eq!(local.steps, named.steps + 3);
    assert_eq!(local.call_depth, 0);
    local.call_depth = 510;
    assert_eq!(
        local
            .call_function_value(function_value_call(&value, 1, 0))
            .unwrap(),
        Value::Int(0)
    );
    assert_eq!(local.call_depth, 510);
    assert_depth_error(local.call_function_value(function_value_call(&value, 2, 0)));
    assert_eq!(local.call_depth, 510);
}

#[test]
fn production_local_self_name_shadowing_remains_legal_without_partial_selection() {
    let mut interpreter = production_interpreter(
        r#"
fn Make() {
    fn local(n: int): int {
        if (n <= 0) { local: int = 7
            return local }
        return local(n - 1)
    }
    return local.clone()
}
"#,
    );
    let value = interpreter
        .call_function("Make", vec![], Span::default(), 0)
        .unwrap();
    assert!(
        try_execute_function_value(&mut interpreter, &function_value_call(&value, 2, 0)).is_none()
    );
    assert_eq!(
        interpreter
            .call_function_value(function_value_call(&value, 2, 0))
            .unwrap(),
        Value::Int(7)
    );
    assert_eq!(interpreter.call_depth, 0);
}

#[test]
fn production_self_name_collision_retains_existing_global_call_precedence() {
    let mut interpreter = production_interpreter(
        r#"
fn collide(n: int): int { return n + 100 }
fn Make() {
    fn collide(n: int): int {
        if (n <= 0) { return 0 }
        return collide(n - 1)
    }
    return collide.clone()
}
"#,
    );
    let value = interpreter
        .call_function("Make", vec![], Span::default(), 0)
        .unwrap();
    assert!(
        try_execute_function_value(&mut interpreter, &function_value_call(&value, 2, 0)).is_none()
    );
    assert_eq!(
        interpreter
            .call_function_value(function_value_call(&value, 2, 0))
            .unwrap(),
        Value::Int(101)
    );
    assert_eq!(interpreter.call_depth, 0);
}

#[test]
fn production_unselected_owned_capture_global_call_and_self_alias_remain_supported() {
    for (source, expected) in [
        (
            r#"
fn Make() {
    prefix = "kept"
    fn local(n: int): int { return prefix.len() + n }
    return local.clone()
}
"#,
            6,
        ),
        (
            r#"
fn Add(n: int): int { return n + 10 }
fn Make() {
    fn local(n: int): int { return Add(n) + 1 }
    return local.clone()
}
"#,
            13,
        ),
        (
            r#"
fn Make() {
    fn local(n: int): int {
        alias = local.clone()
        if (n <= 0) { return 0 }
        return alias(n - 1)
    }
    return local.clone()
}
"#,
            0,
        ),
    ] {
        let mut interpreter = production_interpreter(source);
        let value = interpreter
            .call_function("Make", vec![], Span::default(), 0)
            .unwrap();
        assert!(
            try_execute_function_value(&mut interpreter, &function_value_call(&value, 2, 0))
                .is_none(),
            "the entire unsuitable subtree must keep its original evaluator"
        );
        assert_eq!(
            interpreter
                .call_function_value(function_value_call(&value, 2, 0))
                .unwrap(),
            Value::Int(expected)
        );
        if let Value::Function { captures, .. } = &value {
            if captures.contains("prefix") {
                assert_eq!(
                    captures.get("prefix", Span::default()).unwrap(),
                    Value::String("kept".into())
                );
            }
        }
        assert_eq!(interpreter.call_depth, 0);
    }
}
