use super::*;
use crate::checker::Checker;

const GUARDED_TASK: &str = r#"
async fn Child(): int! { return ok(7) }
async fn main(): null! {
    calls = 0
    fn Probe(): bool { calls += 1 return false }
    handle = Child()
    selected = match handle {
        candidate if (Probe()) => candidate
        candidate => candidate
    }
    if (calls != 1) { panic "guard evaluated more than once" }
    if ((await selected)? != 7) { panic "lost task" }
    return ok(null)
}
"#;

fn checked_program(source: &str) -> Program {
    let program = Parser::new(Lexer::new(source).tokenize().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&program).unwrap();
    program
}

#[test]
fn interpreter_match_false_guard_does_not_cancel_task_and_runs_guard_once() {
    let program = checked_program(GUARDED_TASK);
    let main = program
        .items
        .iter()
        .find_map(|item| match item {
            Item::Function(function) if function.name == "main" => Some(function),
            _ => None,
        })
        .unwrap();
    let selected = main
        .body
        .iter()
        .find_map(|stmt| match stmt {
            Stmt::VarDecl { name, value, .. } | Stmt::Assign { name, value, .. }
                if name == "selected" =>
            {
                Some(value)
            }
            _ => None,
        })
        .unwrap();
    let probe = main
        .body
        .iter()
        .find(|stmt| matches!(stmt, Stmt::Function(function) if function.name == "Probe"))
        .unwrap();
    let runtime = TaskRuntime::new();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let task = runtime
        .spawn(move || {
            ready_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            Ok(stdlib::errors::ok(Value::Int(7)))
        })
        .unwrap();
    ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let mut env = Env::new();
    env.define("calls".into(), Value::Int(0), true, Span::default())
        .unwrap();
    env.define("handle".into(), Value::Task(task), false, Span::default())
        .unwrap();
    let mut interpreter = Interpreter::new();
    interpreter.task_runtime = Some(runtime.clone());
    interpreter.exec_stmt(probe, &mut env, 0).unwrap();
    let result = interpreter.eval(selected, &mut env, 0);
    // Release even after a regression so the bounded fixture cannot strand its
    // worker. The old tentative-owner path returns shutdown_timeout/cancellation.
    release_tx.send(()).unwrap();
    let Value::Task(task) = result.unwrap() else {
        panic!("selected Task was lost")
    };
    assert_eq!(
        task.await_result().unwrap(),
        stdlib::errors::ok(Value::Int(7))
    );
    assert_eq!(env.get("calls", Span::default()).unwrap(), Value::Int(1));
    assert!(env.all_owned_tasks(Span::default()).unwrap().is_empty());
}

#[test]
fn checker_match_guard_rejects_consuming_tentative_task() {
    let source = r#"
        async fn Child(): int! { return ok(7) }
        async fn main(): null! {
            handle = Child()
            selected = match handle {
                candidate if ((await candidate)? == 0) => 0
                candidate => (await candidate)?
            }
            return ok(null)
        }
    "#;
    let program = Parser::new(Lexer::new(source).tokenize().unwrap())
        .parse_program()
        .unwrap();
    let error = Checker::new()
        .check(&program)
        .expect_err("a false guard must not consume the task needed by later arms");
    assert!(error.message.contains("tentative Task binding"), "{error}");
}

#[test]
fn checker_match_guard_can_create_and_await_its_own_task() {
    checked_program(
        r#"
        async fn Child(): int! { return ok(7) }
        async fn main(): null! {
            handle = Child()
            selected = match handle {
                candidate if ((await Child())? == 7) => candidate
                candidate => candidate
            }
            value = (await selected)?
            return ok(null)
        }
    "#,
    );
}

#[test]
fn checker_match_task_binding_consumes_original_scrutinee() {
    for input in ["handle", "[handle]"] {
        let source = format!(
            r#"
            async fn Child(): int! {{ return ok(7) }}
            async fn main(): null! {{
                handle = Child()
                original = {input}
                selected = match original {{ candidate => candidate }}
                again = original
                return ok(null)
            }}
        "#
        );
        let program = Parser::new(Lexer::new(&source).tokenize().unwrap())
            .parse_program()
            .unwrap();
        let error = Checker::new()
            .check(&program)
            .expect_err("match must transfer Task ownership out of the original slot");
        assert!(
            error.message.contains("moved") && error.message.contains("original"),
            "{error}"
        );
    }
}

#[test]
fn interpreter_match_probe_blocks_owning_read_until_selection_and_moves_only_payload() {
    let runtime = TaskRuntime::new();
    let first = runtime.spawn(|| Ok(Value::Int(1))).unwrap();
    let second = runtime.spawn(|| Ok(Value::Int(2))).unwrap();
    let first_id = first.id();
    let second_id = second.id();
    let mut value = Value::Enum {
        name: "InternalPair".into(),
        variant: "Both".into(),
        fields: vec![Value::Task(first), Value::Task(second)],
    };
    // This direct runtime-value fixture tests nested projection ownership, not
    // a new Ku annotation spelling for Task-containing enum payloads.
    let pattern = MatchPattern::EnumVariant {
        enum_name: "InternalPair".into(),
        variant: "Both".into(),
        fields: vec![
            MatchPattern::Binding("chosen".into()),
            MatchPattern::Wildcard,
        ],
    };
    let span = Span::default();
    let mut env = Env::new();
    env.push_scope();
    let plan = match_pattern(&pattern, &value, &mut env, span)
        .unwrap()
        .unwrap();
    assert!(env.current_scope_owned_tasks(span).unwrap().is_empty());
    let error = env.get_owning("chosen", span).unwrap_err();
    assert_eq!(
        error.diagnostic_id(),
        crate::error::DiagnosticId::TaskGuardMove
    );
    assert!(value.contains_owned_task(span).unwrap());
    for (name, path) in plan {
        let selected = value.take_task_projection(&path, span).unwrap().unwrap();
        env.commit_task_match_probe(&name, selected, span).unwrap();
    }
    let Value::Task(chosen) = env.get_owning("chosen", span).unwrap() else {
        unreachable!()
    };
    assert_eq!(chosen.id(), first_id);
    let mut remaining = Vec::new();
    value.collect_owned_tasks(&mut remaining, span).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id(), second_id);
    assert!(env.current_scope_owned_tasks(span).unwrap().is_empty());
}

#[test]
fn interpreter_match_wildcard_waits_for_running_task_cleanup_before_return() {
    checked_program(
        r#"
        async fn Child(): null! { return ok(null) }
        async fn main(): null! {
            handle = Child()
            selected = match handle { _ => 0 }
            return ok(null)
        }
        "#,
    );
    let runtime = TaskRuntime::new();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (cleanup_tx, cleanup_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let task = runtime
        .spawn(move || {
            ready_tx.send(()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            while !crate::runtime::task::current_task_cancelled() {
                assert!(
                    Instant::now() < deadline,
                    "wildcard never cancelled its Task"
                );
                thread::yield_now();
            }
            cleanup_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            Ok(Value::Null)
        })
        .unwrap();
    ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let (returned_tx, returned_rx) = mpsc::sync_channel(1);
    let evaluation_runtime = runtime.clone();
    let evaluation = thread::spawn(move || {
        let expression = Parser::new(Lexer::new("match handle { _ => 0 }").tokenize().unwrap())
            .parse_expression_only()
            .unwrap();
        let mut interpreter = Interpreter::new();
        interpreter.task_runtime = Some(evaluation_runtime);
        let mut env = Env::new();
        env.define_owned("handle".into(), Value::Task(task), false, Span::default())
            .unwrap();
        let result = interpreter.eval(&expression, &mut env, 0);
        returned_tx.send(()).unwrap();
        assert_eq!(env.get("handle", Span::default()).unwrap(), Value::Null);
        assert!(env.all_owned_tasks(Span::default()).unwrap().is_empty());
        result
    });
    cleanup_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    // The child is RUNNING and held inside cleanup: queue cancellation and a
    // nonblocking owner drop cannot satisfy this expression-level join contract.
    let returned_while_cleanup_blocked = returned_rx.recv_timeout(Duration::from_millis(30));
    release_tx.send(()).unwrap();
    let result = evaluation.join().unwrap();
    assert!(matches!(
        returned_while_cleanup_blocked,
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    assert_eq!(result.unwrap(), Value::Int(0));
}

#[test]
fn interpreter_match_selected_task_survives_while_wildcard_sibling_is_joined() {
    let runtime = TaskRuntime::new();
    let (chosen_ready_tx, chosen_ready_rx) = mpsc::sync_channel(1);
    let (chosen_release_tx, chosen_release_rx) = mpsc::sync_channel(1);
    let chosen = runtime
        .spawn(move || {
            chosen_ready_tx.send(()).unwrap();
            chosen_release_rx
                .recv_timeout(Duration::from_secs(3))
                .unwrap();
            Ok(Value::Int(7))
        })
        .unwrap();
    chosen_ready_rx
        .recv_timeout(Duration::from_secs(3))
        .unwrap();
    let chosen_id = chosen.id();
    let (sibling_ready_tx, sibling_ready_rx) = mpsc::sync_channel(1);
    let (sibling_cleanup_tx, sibling_cleanup_rx) = mpsc::sync_channel(1);
    let (sibling_release_tx, sibling_release_rx) = mpsc::sync_channel(1);
    let sibling = runtime
        .spawn(move || {
            sibling_ready_tx.send(()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            while !crate::runtime::task::current_task_cancelled() {
                assert!(
                    Instant::now() < deadline,
                    "wildcard sibling was not cancelled"
                );
                thread::yield_now();
            }
            sibling_cleanup_tx.send(()).unwrap();
            sibling_release_rx
                .recv_timeout(Duration::from_secs(3))
                .unwrap();
            Ok(Value::Null)
        })
        .unwrap();
    sibling_ready_rx
        .recv_timeout(Duration::from_secs(3))
        .unwrap();
    let (returned_tx, returned_rx) = mpsc::sync_channel(1);
    let evaluation_runtime = runtime.clone();
    let evaluation = thread::spawn(move || {
        // The enum layout is an internal runtime fixture, not a new public
        // Task-containing type annotation. The pattern uses existing syntax.
        let expression = Parser::new(
            Lexer::new("match pair { InternalPair.Both(chosen, _) => chosen }")
                .tokenize()
                .unwrap(),
        )
        .parse_expression_only()
        .unwrap();
        let mut env = Env::new();
        env.define_owned(
            "pair".into(),
            Value::Enum {
                name: "InternalPair".into(),
                variant: "Both".into(),
                fields: vec![Value::Task(chosen), Value::Task(sibling)],
            },
            false,
            Span::default(),
        )
        .unwrap();
        let mut interpreter = Interpreter::new();
        interpreter.task_runtime = Some(evaluation_runtime);
        let result = interpreter.eval(&expression, &mut env, 0);
        returned_tx.send(()).unwrap();
        result
    });
    sibling_cleanup_rx
        .recv_timeout(Duration::from_secs(3))
        .unwrap();
    let returned_while_cleanup_blocked = returned_rx.recv_timeout(Duration::from_millis(30));
    sibling_release_tx.send(()).unwrap();
    let result = evaluation.join().unwrap();
    // Always release the chosen worker, including on an incorrect cancellation
    // result, so an assertion cannot strand the test's bounded worker.
    chosen_release_tx.send(()).unwrap();
    assert!(matches!(
        returned_while_cleanup_blocked,
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    let Value::Task(chosen) = result.unwrap() else {
        panic!("selected Task was lost")
    };
    assert_eq!(chosen.id(), chosen_id);
    assert_eq!(chosen.await_result().unwrap(), Value::Int(7));
}

#[test]
fn interpreter_match_arm_and_residual_tasks_share_exact_cleanup_context() {
    let runtime = TaskRuntime::new();
    let (ready_tx, ready_rx) = mpsc::sync_channel(2);
    let (cancelled_tx, cancelled_rx) = mpsc::sync_channel(2);
    let spawn_child = || {
        let ready_tx = ready_tx.clone();
        let cancelled_tx = cancelled_tx.clone();
        runtime
            .spawn(move || {
                ready_tx.send(()).unwrap();
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    if let Some(context) = crate::runtime::task::current_task_cancellation() {
                        cancelled_tx.send(context).unwrap();
                        return Ok(Value::Null);
                    }
                    assert!(Instant::now() < deadline, "match child was not cancelled");
                    thread::yield_now();
                }
            })
            .unwrap()
    };
    let bound = spawn_child();
    let residual = spawn_child();
    let bound_observer = bound.clone();
    let residual_observer = residual.clone();
    for _ in 0..2 {
        ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    }
    // Both workers are running. The selected binding remains in the arm scope;
    // the wildcard remains in the owned scrutinee until that arm has cleaned up.
    let expression = Parser::new(
        Lexer::new("match pair { InternalPair.Both(bound, _) => 0 }")
            .tokenize()
            .unwrap(),
    )
    .parse_expression_only()
    .unwrap();
    let mut env = Env::new();
    env.define_owned(
        "pair".into(),
        Value::Enum {
            name: "InternalPair".into(),
            variant: "Both".into(),
            fields: vec![Value::Task(bound), Value::Task(residual)],
        },
        false,
        Span::default(),
    )
    .unwrap();
    let mut interpreter = Interpreter::new();
    interpreter.task_runtime = Some(runtime.clone());
    assert_eq!(
        interpreter.eval(&expression, &mut env, 0).unwrap(),
        Value::Int(0)
    );
    assert_eq!(bound_observer.status(), "cancelled");
    assert_eq!(residual_observer.status(), "cancelled");
    let arm_context = cancelled_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let residual_context = cancelled_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(arm_context.reason, TerminationReason::Cancelled);
    assert_eq!(
        arm_context, residual_context,
        "arm and scrutinee cleanup must use the same reason and absolute deadline"
    );
    for task in [bound_observer, residual_observer] {
        let Value::Result { ok: false, value } = task.await_result().unwrap() else {
            panic!("a scope-released observer must not retain an awaitable result")
        };
        let Value::Object(fields) = *value else {
            panic!("expected a structured task error")
        };
        assert_eq!(fields.get("domain"), Some(&Value::String("task".into())));
        assert_eq!(
            fields.get("code"),
            Some(&Value::String("already_awaited".into()))
        );
    }
    assert!(env.all_owned_tasks(Span::default()).unwrap().is_empty());
}

fn nested_pattern_fixture(
    depth: usize,
    mut pattern: MatchPattern,
    mut value: Value,
) -> (MatchPattern, Value) {
    for _ in 0..depth {
        pattern = MatchPattern::EnumVariant {
            enum_name: "Nested".into(),
            variant: "Only".into(),
            fields: vec![pattern],
        };
        value = Value::Enum {
            name: "Nested".into(),
            variant: "Only".into(),
            fields: vec![value],
        };
    }
    (pattern, value)
}

fn drop_nested_match_value(mut value: Value) {
    loop {
        match value {
            Value::Enum { mut fields, .. } if fields.len() == 1 => {
                value = fields.pop().unwrap();
            }
            value => {
                drop(value);
                return;
            }
        }
    }
}

fn bounded_pattern_child(test_name: &str) -> bool {
    const CHILD: &str = "KU_RUNTIME_MATCH_PATTERN_CHILD";
    if std::env::var(CHILD).as_deref() == Ok(test_name) {
        return true;
    }
    use crate::registry_server::native_test_harness::{run_bounded, OutputLimits};
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD, test_name);
    let output = run_bounded(
        &mut command,
        Duration::from_secs(20),
        OutputLimits::new(64 * 1024, 128 * 1024),
    )
    .expect("match pattern regression has a bounded subprocess");
    assert!(
        output.status.success(),
        "{:?}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let completed = format!("match pattern regression completed: {test_name}");
    assert!(String::from_utf8_lossy(&output.stderr)
        .lines()
        .any(|line| line == completed));
    false
}

#[test]
fn interpreter_deep_match_pattern_runs_on_worker_with_direct_and_borrowed_inputs() {
    const CHILD: &str = "KU_RUNTIME_DEEP_MATCH_PATTERN_CHILD";
    const COMPLETED: &str = "runtime deep match pattern regression completed";
    if std::env::var_os(CHILD).is_none() {
        use crate::registry_server::native_test_harness::{run_bounded, OutputLimits};
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "runtime::interpreter::match_task_tests::interpreter_deep_match_pattern_runs_on_worker_with_direct_and_borrowed_inputs",
                "--nocapture",
            ])
            .env(CHILD, "1");
        let output = run_bounded(
            &mut command,
            Duration::from_secs(20),
            OutputLimits::new(64 * 1024, 128 * 1024),
        )
        .expect("deep match pattern has a bounded subprocess");
        assert!(
            output.status.success(),
            "deep match pattern subprocess failed: {:?}\n{}{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stderr)
            .lines()
            .any(|line| line == COMPLETED));
        return;
    }
    let runtime = TaskRuntime::with_limits(1, 1, 0, 0, 2);
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let job = runtime
        .spawn(move || {
            assert!(thread::current().name().unwrap().starts_with("ku-task-"));
            let span = Span::default();
            for borrowed in [false, true] {
                let (pattern, value) = nested_pattern_fixture(
                    4_000,
                    MatchPattern::Binding("selected".into()),
                    Value::Int(7),
                );
                let root = Arc::new(value);
                let view = Value::Borrowed(BorrowedValue::new(&root));
                let input = if borrowed { &view } else { root.as_ref() };
                let mut env = Env::new();
                assert!(match_pattern(&pattern, input, &mut env, span)?.is_some());
                assert_eq!(env.get("selected", span)?, Value::Int(7));
                assert!(env.all_owned_tasks(span)?.is_empty());
                assert_eq!(Arc::strong_count(&root), 1);
                drop(view);
                let value = match Arc::try_unwrap(root) {
                    Ok(value) => value,
                    Err(_) => panic!("match projection retained its source"),
                };
                drop_nested_match_value(value);
                drop(pattern);
            }
            done_tx.send(thread::current().id()).unwrap();
            Ok(Value::Null)
        })
        .unwrap();
    let worker = done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(job.await_result().unwrap(), Value::Null);
    let (healthy_tx, healthy_rx) = mpsc::sync_channel(1);
    let healthy = runtime
        .spawn(move || {
            assert_eq!(current_cleanup_context(), None);
            healthy_tx.send(thread::current().id()).unwrap();
            Ok(Value::Int(19))
        })
        .unwrap();
    assert_eq!(
        healthy_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        worker
    );
    assert_eq!(healthy.await_result().unwrap(), Value::Int(19));
    runtime.cancel_all_and_wait(Duration::from_secs(1)).unwrap();
    assert_eq!(runtime.snapshot().unwrap().active_tasks, 0);
    eprintln!("{COMPLETED}");
}

#[test]
fn interpreter_match_pattern_false_and_error_restore_path_with_original_binding_rules() {
    let test_name = "runtime::interpreter::match_task_tests::interpreter_match_pattern_false_and_error_restore_path_with_original_binding_rules";
    if !bounded_pattern_child(test_name) {
        return;
    }
    let span = Span::default();
    for error in [false, true] {
        let (leaf_pattern, leaf_value) = if error {
            (
                MatchPattern::EnumVariant {
                    enum_name: "Tail".into(),
                    variant: "Only".into(),
                    fields: vec![MatchPattern::Wildcard, MatchPattern::Wildcard],
                },
                Value::Enum {
                    name: "Tail".into(),
                    variant: "Only".into(),
                    fields: vec![Value::Int(7)],
                },
            )
        } else {
            (MatchPattern::Literal(Literal::Int(8)), Value::Int(7))
        };
        let (nested, nested_value) = nested_pattern_fixture(4_000, leaf_pattern, leaf_value);
        let pattern = MatchPattern::EnumVariant {
            enum_name: "Pair".into(),
            variant: "Both".into(),
            fields: vec![MatchPattern::Binding("first".into()), nested],
        };
        let value = Value::Enum {
            name: "Pair".into(),
            variant: "Both".into(),
            fields: vec![Value::Int(3), nested_value],
        };
        let mut bindings = vec![MatchBinding {
            name: "original".into(),
            value: Value::Int(42),
            task_path: None,
        }];
        let mut path = vec![ValueProjection::Field("original-path".into())];
        let result = collect_match_bindings(&pattern, &value, &mut bindings, &mut path, span);
        if error {
            assert!(result
                .unwrap_err()
                .message
                .contains("expects 1 fields but got 2"));
            assert_eq!(bindings.len(), 2);
            assert_eq!(bindings[1].name, "first");
            assert_eq!(bindings[1].value, Value::Int(3));
        } else {
            assert!(!result.unwrap());
            assert_eq!(bindings.len(), 1);
        }
        assert_eq!(bindings[0].name, "original");
        assert_eq!(bindings[0].value, Value::Int(42));
        assert!(matches!(&path[..], [ValueProjection::Field(name)] if name == "original-path"));
        let Value::Enum { mut fields, .. } = value else {
            unreachable!()
        };
        drop_nested_match_value(fields.pop().unwrap());
    }

    // A borrowed root remains a view even when its source is a scalar. Only
    // an enum child projection was converted to Copy by the previous matcher.
    let root = Arc::new(Value::Int(7));
    let view = Value::Borrowed(BorrowedValue::new(&root));
    let mut bindings = Vec::new();
    let mut path = Vec::new();
    assert!(collect_match_bindings(
        &MatchPattern::Binding("root".into()),
        &view,
        &mut bindings,
        &mut path,
        span,
    )
    .unwrap_err()
    .message
    .contains("owned borrowed match payload"));
    drop(root);
    assert!(!collect_match_bindings(
        &MatchPattern::Literal(Literal::Int(7)),
        &view,
        &mut bindings,
        &mut path,
        span,
    )
    .unwrap());
    assert!(collect_match_bindings(
        &MatchPattern::Wildcard,
        &view,
        &mut bindings,
        &mut path,
        span,
    )
    .unwrap());
    assert!(bindings.is_empty());
    assert!(path.is_empty());

    let root = Arc::new(Value::Enum {
        name: "Pair".into(),
        variant: "Both".into(),
        fields: vec![Value::Int(3), Value::String("owned".into())],
    });
    let view = Value::Borrowed(BorrowedValue::new(&root));
    let pattern = MatchPattern::EnumVariant {
        enum_name: "Pair".into(),
        variant: "Both".into(),
        fields: vec![
            MatchPattern::Binding("first".into()),
            MatchPattern::Binding("second".into()),
        ],
    };
    assert!(
        collect_match_bindings(&pattern, &view, &mut bindings, &mut path, span)
            .unwrap_err()
            .message
            .contains("owned borrowed match payload")
    );
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].name, "first");
    assert_eq!(bindings[0].value, Value::Int(3));
    assert!(path.is_empty());
    assert_eq!(Arc::strong_count(&root), 1);
    eprintln!("match pattern regression completed: {test_name}");
}

#[test]
fn interpreter_match_pattern_capture_cost_and_scratch_rejection_keep_original_owners() {
    let test_name = "runtime::interpreter::match_task_tests::interpreter_match_pattern_capture_cost_and_scratch_rejection_keep_original_owners";
    if !bounded_pattern_child(test_name) {
        return;
    }
    let span = Span::default();
    let (pattern, value) =
        nested_pattern_fixture(40_000, MatchPattern::Binding("leaf".into()), Value::Int(7));
    let mut bindings = Vec::new();
    let mut path = vec![ValueProjection::Field("kept".into())];
    let error =
        collect_match_bindings(&pattern, &value, &mut bindings, &mut path, span).unwrap_err();
    assert!(error
        .message
        .contains("match traversal scratch budget exhausted"));
    assert!(bindings.is_empty());
    assert!(matches!(&path[..], [ValueProjection::Field(name)] if name == "kept"));
    // Failure did not move the borrowed input tree. Its complete original
    // chain and scalar remain available, then retire iteratively in this test.
    let mut current = &value;
    let mut depth = 0;
    while let Value::Enum { fields, .. } = current {
        assert_eq!(fields.len(), 1);
        depth += 1;
        current = &fields[0];
    }
    assert_eq!(depth, 40_000);
    assert_eq!(current, &Value::Int(7));
    let mut budget = CaptureScanBudget::new();
    let mut bound = HashSet::new();
    let error = bind_match_pattern_names(&pattern, &mut bound, &mut budget, 7, span).unwrap_err();
    assert!(error.message.contains("nesting depth exceeded"));
    assert_eq!(
        budget.remaining_work,
        MAX_CAPTURE_SCAN_WORK - (MAX_CAPTURE_SCAN_DEPTH - 7)
    );
    assert!(bound.is_empty());
    drop_nested_match_value(value);
    drop(pattern);

    let pattern = MatchPattern::EnumVariant {
        enum_name: "Pair".into(),
        variant: "Both".into(),
        fields: vec![
            MatchPattern::Binding("first".into()),
            MatchPattern::EnumVariant {
                enum_name: "Inner".into(),
                variant: "Only".into(),
                fields: vec![MatchPattern::Binding("second".into())],
            },
        ],
    };
    let mut budget = CaptureScanBudget::new();
    bind_match_pattern_names(&pattern, &mut bound, &mut budget, 0, span).unwrap();
    assert_eq!(bound, HashSet::from(["first".into(), "second".into()]));
    assert_eq!(budget.remaining_work, MAX_CAPTURE_SCAN_WORK - 4);
    eprintln!("match pattern regression completed: {test_name}");
}
