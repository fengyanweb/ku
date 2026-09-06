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
