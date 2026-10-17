use super::*;

#[test]
fn interpreter_task_worker_recursion_has_structured_depth_limit() {
    const CHILD: &str = "KU_TASK_WORKER_RECURSION_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "runtime::interpreter::task_cleanup_tests::interpreter_task_worker_recursion_has_structured_depth_limit",
                "--nocapture",
            ])
            .env(CHILD, "1");
        let output = finally_bounded_process::run_bounded(
            &mut command,
            Duration::from_secs(15),
            finally_bounded_process::OutputLimits::new(64 * 1024, 128 * 1024),
        )
        .expect("worker recursion cannot hang the test supervisor");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let source = r#"
fn Descend(n: int): int {
    if (n <= 0) { return 0 }
    return Descend(n - 1)
}
fn main() {}
"#;
    let program = Parser::new(Lexer::new(source).tokenize().unwrap())
        .parse_program()
        .unwrap();
    crate::checker::Checker::new().check(&program).unwrap();
    let mut interpreter = Interpreter::new();
    for item in program.items {
        let Item::Function(function) = item else {
            panic!("recursion fixture contains only functions");
        };
        interpreter
            .functions
            .insert(function.name.clone(), function);
    }
    let runtime = TaskRuntime::new();
    interpreter.task_runtime = Some(runtime.clone());
    interpreter.async_execution = true;
    let parent_id = thread::current().id();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let task = runtime.spawn(move || {
        let worker = thread::current();
        let _ = ready_tx.send((worker.id(), worker.name().unwrap_or("").to_string()));
        let result =
            interpreter.call_function("Descend", vec![Value::Int(1024)], Span::default(), 0);
        let _ = result_tx.send((result, interpreter.call_depth));
        Ok(Value::Null)
    });
    // Neither await_result nor help_one may run until the worker's result arrives.
    let ready = ready_rx.recv_timeout(Duration::from_secs(3));
    if let Ok((id, name)) = &ready {
        eprintln!("worker recursion entered: {name} {id:?}");
    }
    let outcome = result_rx.recv_timeout(Duration::from_secs(3));
    let admission_error = task.as_ref().err().cloned();
    // There is only one Task owner. Ordinary failure paths also drain before assertions.
    drop(task);
    let drained = runtime.cancel_all_and_wait(Duration::from_secs(2));
    let snapshot = runtime.snapshot();
    drained.expect("worker recursion task must drain");
    let snapshot = snapshot.unwrap();
    assert!(admission_error.is_none(), "{admission_error:?}");
    let (worker_id, worker_name) = ready.expect("a real worker must report entry");
    assert_ne!(worker_id, parent_id);
    assert!(worker_name.starts_with("ku-task-"), "{worker_name}");
    let (result, depth) = outcome.expect("worker recursion must finish within the channel bound");
    let error = result.expect_err("finite recursion past MAX_CALL_DEPTH must reject");
    assert_eq!(error.domain.as_deref(), Some("runtime"));
    assert_eq!(error.code.as_deref(), Some("call_depth_exceeded"));
    assert_eq!(depth, 0);
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(snapshot.queued_blocking_jobs, 0);
    assert_eq!(snapshot.running_blocking_jobs, 0);
}

fn wait_marker(env: &Env, expected: i64) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while finally_tests::marker(env) != expected {
        assert!(
            Instant::now() < deadline,
            "task did not reach its cancellation barrier"
        );
        thread::yield_now();
    }
}

#[test]
fn interpreter_task_cancel_finally_is_uncatchable_and_preserves_outer_cleanup() {
    const CHILD: &str = "KU_TASK_CLEANUP_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "runtime::interpreter::task_cleanup_tests::interpreter_task_cancel_finally_is_uncatchable_and_preserves_outer_cleanup", "--nocapture"])
            .env(CHILD, "1");
        let output = finally_bounded_process::run_bounded(
            &mut command,
            Duration::from_secs(15),
            finally_bounded_process::OutputLimits::new(64 * 1024, 128 * 1024),
        )
        .expect("cancellation cleanup cannot hang the test process");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    for (source, expected) in [
        ("try { marker = 1\nwhile (true) {} } catch (err) { marker = 9 } finally { marker = 2 }", 2),
        ("try { marker = 1\nwhile (true) {} } finally { marker = 2\nreturn [\"late success\"] }", 2),
        ("try { marker = 1\nwhile (true) {} } finally { marker = 2\nfail \"cleanup failure\" }", 2),
        ("try { try { marker = 1\nwhile (true) {} } finally { marker = 2\npanic(\"cleanup panic\") } } finally { marker = marker * 10 + 3 }", 23),
        ("try { try { marker = 1\nwhile (true) {} } finally { marker = 2\nwhile (true) {} } } finally { marker = 9 }", 2),
    ] {
        let runtime = TaskRuntime::new();
        let child_runtime = runtime.clone();
        let body = finally_tests::body(source);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let task = runtime.spawn(move || {
            let mut env = finally_tests::marker_env();
            ready_tx.send(env.clone()).unwrap();
            let mut interpreter = Interpreter::new();
            interpreter.task_runtime = Some(child_runtime);
            interpreter.async_execution = true;
            interpreter.exec_block(&body, &mut env, 0).map(|_| Value::Null)
        }).unwrap();
        let env = ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        wait_marker(&env, 1);
        assert!(task.cancel());
        let error = task.await_result().expect_err("cancel cannot become a recoverable Result or success");
        assert_eq!(error.runtime_termination().unwrap().reason, TerminationReason::Cancelled);
        assert_eq!(finally_tests::marker(&env), expected, "{source}");
        runtime.cancel_all_and_wait(Duration::from_secs(2)).unwrap();
        let snapshot = runtime.snapshot().unwrap();
        assert_eq!(snapshot.active_tasks, 0);
        assert_eq!(snapshot.running_blocking_jobs, 0);
    }
}

#[test]
fn interpreter_task_cleanup_rejects_new_async_await_and_blocking_before_submission() {
    let runtime = TaskRuntime::new();
    let mut interpreter = Interpreter::new();
    interpreter.task_runtime = Some(runtime.clone());
    let context = CancellationContext::new(TerminationReason::Cancelled);
    let guard = CleanupGuard::enter(context);
    for source in [
        "await 1",
        "time.sleep(100000)",
        "fs.write(\"must-not-create-ku-cleanup-file\", \"bad\")",
    ] {
        let program = Parser::new(
            Lexer::new(&format!("fn main() {{ {source} }}"))
                .tokenize()
                .unwrap(),
        )
        .parse_program()
        .unwrap();
        let Item::Function(function) = program.items.into_iter().next().unwrap() else {
            unreachable!()
        };
        let mut env = Env::new();
        interpreter
            .std_modules
            .extend(["time".to_string(), "fs".to_string()]);
        let error = interpreter
            .exec_block(&function.body, &mut env, 0)
            .err()
            .expect("cleanup operation must reject");
        assert!(error.runtime_termination().is_some(), "{source}: {error}");
    }
    let program = Parser::new(
        Lexer::new("async fn Child(): null! { return ok(null) }")
            .tokenize()
            .unwrap(),
    )
    .parse_program()
    .unwrap();
    let Item::Function(function) = program.items.into_iter().next().unwrap() else {
        unreachable!()
    };
    let before = runtime.snapshot().unwrap();
    assert!(interpreter
        .spawn_async_function(function, Vec::new(), Span::default())
        .unwrap_err()
        .runtime_termination()
        .is_some());
    let after = runtime.snapshot().unwrap();
    assert_eq!(before.total_submissions, after.total_submissions);
    assert_eq!(after.queued_blocking_jobs, 0);
    assert_eq!(after.running_blocking_jobs, 0);
    drop(guard);
}

#[test]
fn interpreter_task_termination_marker_cannot_be_forged_by_error_fields() {
    let ordinary = KuError::structured(
        crate::error::KuErrorKind::Runtime,
        "task",
        "cancelled",
        "async task was cancelled",
        Span::default(),
    );
    assert!(ordinary.runtime_termination().is_none());
    let context = CancellationContext::new(TerminationReason::TimedOut);
    let internal = KuError::termination(context, Span::default());
    assert_eq!(internal.clone().runtime_termination(), Some(context));
    let attached = internal.with_diagnostic_context("fixture.ku", "while (true) {}");
    assert_eq!(attached.clone().runtime_termination(), Some(context));
    assert!(attached
        .diagnostic("fallback.ku", "")
        .contains("fixture.ku"));
    assert!(
        std::mem::size_of::<KuError>() <= 120,
        "cold termination metadata must not grow every successful KuResult"
    );
}

#[test]
fn interpreter_http_timeout_temporary_argument_keeps_original_termination_context() {
    let runtime = TaskRuntime::new();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (cancel_tx, cancel_rx) = mpsc::sync_channel(1);
    let child = runtime
        .spawn(move || {
            ready_tx.send(()).unwrap();
            let limit = Instant::now() + Duration::from_secs(3);
            loop {
                if let Some(context) = current_task_cancellation() {
                    cancel_tx.send(context).unwrap();
                    return Err(KuError::termination(context, Span::default()));
                }
                assert!(
                    Instant::now() < limit,
                    "temporary argument Task owner was not cancelled"
                );
                thread::yield_now();
            }
        })
        .unwrap();
    ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let program = Parser::new(
        Lexer::new(
            r#"
        fn Spin(): int { while (true) {} return 0 }
        fn Consume(a: int, b: int): null { return null }
        fn Handler(): null { Consume(child, Spin()) return null }
    "#,
        )
        .tokenize()
        .unwrap(),
    )
    .parse_program()
    .unwrap();
    let mut interpreter = Interpreter::new();
    interpreter.task_runtime = Some(runtime.clone());
    let mut handler_body = Vec::new();
    for item in program.items {
        if let Item::Function(function) = item {
            if function.name == "Handler" {
                handler_body = function.body;
            } else {
                interpreter
                    .functions
                    .insert(function.name.clone(), function);
            }
        }
    }
    // Runtime-only fixture: the first argument is moved out of its source slot,
    // then the second argument times out before the callee can accept ownership.
    let handler = Value::Function {
        params: vec!["child".into()],
        param_modes: vec![ParamMode::Owned],
        body: handler_body,
        captures: Env::new(),
        self_name: None,
        is_async: false,
    };
    let error = interpreter
        .call_http_handler(
            handler,
            Value::Task(child),
            Span::default(),
            Instant::now() + Duration::from_millis(20),
        )
        .unwrap_err();
    let original = error
        .runtime_termination()
        .unwrap_or_else(|| panic!("HTTP timeout lost its internal termination: {error}"));
    assert_eq!(original.reason, TerminationReason::TimedOut);
    assert_eq!(
        cancel_rx.recv_timeout(Duration::from_secs(3)).unwrap(),
        original
    );
    assert!(crate::runtime::task::current_execution_termination().is_none());
    assert!(interpreter.termination.is_none());
    runtime.cancel_all_and_wait(Duration::from_secs(2)).unwrap();
}

#[test]
fn interpreter_scope_exit_carries_one_deadline_without_cancelling_continuing_parent() {
    assert_scope_exit_carries_one_deadline(Flow::returned(Value::Int(7)));
}

#[test]
fn interpreter_break_carries_one_deadline_without_cancelling_continuing_parent() {
    assert_scope_exit_carries_one_deadline(Flow::Break(None));
}

#[test]
fn interpreter_loop_continue_carries_one_deadline_without_cancelling_continuing_parent() {
    assert_scope_exit_carries_one_deadline(Flow::LoopContinue(None));
}

fn assert_scope_exit_carries_one_deadline(flow: Flow) {
    let expected_flow = std::mem::discriminant(&flow);
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::new();
    let mut interpreter = Interpreter::new();
    interpreter.task_runtime = Some(runtime.clone());
    let mut env = Env::new();
    for _ in 0..2 {
        env.push_scope();
        let task = runtime
            .spawn(|| Ok(Value::String("completed but not awaited".into())))
            .unwrap();
        env.define_owned("child".into(), Value::Task(task), false, Span::default())
            .unwrap();
    }
    let started = Instant::now();
    let flow = interpreter
        .finish_owned_scope(&mut env, Ok(flow), Span::default(), true)
        .unwrap();
    let original = cancellation::ScopeExit::cleanup_context(&flow).unwrap();
    let finished = Instant::now();
    assert!(original.cleanup_deadline >= started + Duration::from_secs(1));
    assert!(original.cleanup_deadline <= finished + Duration::from_secs(1));
    let flow = interpreter
        .finish_owned_scope(&mut env, Ok(flow), Span::default(), true)
        .unwrap();
    assert_eq!(
        cancellation::ScopeExit::cleanup_context(&flow),
        Some(original)
    );
    assert_eq!(std::mem::discriminant(&flow), expected_flow);
    if let Flow::Return { value, .. } = &flow {
        assert_eq!(value, &Value::Int(7));
    }
    assert!(!env.contains("child"));
    assert!(interpreter.termination.is_none());
    assert!(current_cleanup_context().is_none());
    assert!(current_task_cancellation().is_none());
    assert!(crate::runtime::task::current_execution_termination().is_none());
    interpreter
        .exec_block(&finally_tests::body("return 9"), &mut env, 0)
        .unwrap();
    let next = runtime.spawn(|| Ok(Value::Int(11))).unwrap();
    assert_eq!(next.await_result().unwrap(), Value::Int(11));
    // The consumed loop flow must not cancel a later independent Task scope or
    // shorten its newly created, unchanged one-second cleanup budget.
    drop(flow);
    env.push_scope();
    let independent = runtime.spawn(|| Ok(Value::Null)).unwrap();
    env.define_owned(
        "independent".into(),
        Value::Task(independent),
        false,
        Span::default(),
    )
    .unwrap();
    let independent_started = Instant::now();
    let independent_flow = interpreter
        .finish_owned_scope(
            &mut env,
            Ok(Flow::returned(Value::Null)),
            Span::default(),
            true,
        )
        .unwrap();
    let independent_finished = Instant::now();
    let independent_context = cancellation::ScopeExit::cleanup_context(&independent_flow).unwrap();
    assert!(independent_context.cleanup_deadline >= independent_started + Duration::from_secs(1));
    assert!(independent_context.cleanup_deadline <= independent_finished + Duration::from_secs(1));
    assert!(!env.contains("independent"));
    assert!(interpreter.termination.is_none());
    assert!(current_cleanup_context().is_none());
    assert!(current_task_cancellation().is_none());
    assert!(crate::runtime::task::current_execution_termination().is_none());
    runtime.cancel_all_and_wait(Duration::from_secs(2)).unwrap();
}

fn assert_expired_loop_exit_rejects_at_scope_boundary(is_continue: bool, has_owned_value: bool) {
    // Seeded cleanup-boundary oracle, not a simulated or elapsed HTTP timeout.
    let _execution = ExecutionTerminationGuard::enter();
    let mut interpreter = Interpreter::new();
    let mut env = Env::new();
    env.push_scope();
    if has_owned_value {
        env.define_owned(
            "owned".into(),
            Value::String("outer owned value".into()),
            false,
            Span::default(),
        )
        .unwrap();
    }
    let expired = RuntimeTermination {
        reason: TerminationReason::Cancelled,
        cleanup_deadline: Instant::now()
            .checked_sub(Duration::from_millis(1))
            .expect("test clock must support a past instant"),
    };
    let flow = if is_continue {
        Flow::LoopContinue(Some(expired))
    } else {
        Flow::Break(Some(expired))
    };
    // No ambient cancellation or new Task supplies this context:
    // only this single nonlocal exit carries the already-expired D.
    assert!(interpreter.termination.is_none());
    assert!(current_task_cancellation().is_none());
    assert!(crate::runtime::task::current_cleanup_context().is_none());
    assert!(crate::runtime::task::current_execution_termination().is_none());
    let error = interpreter
        .finish_owned_scope(&mut env, Ok(flow), Span::default(), true)
        .err()
        .expect("an expired nonlocal cleanup must not become successful loop flow");
    assert_eq!(error.domain.as_deref(), Some("task"));
    assert_eq!(error.code.as_deref(), Some("shutdown_timeout"));
    assert_eq!(error.scope_cleanup(), Some(expired));
    assert!(error.runtime_termination().is_none());
    assert!(interpreter.termination.is_none());
    assert!(current_task_cancellation().is_none());
    assert!(crate::runtime::task::current_cleanup_context().is_none());
    assert!(crate::runtime::task::current_execution_termination().is_none());

    // A rejected local cleanup is not permanent parent cancellation.
    // A new empty ordinary scope has no carried D and remains legal.
    env.push_scope();
    let next = interpreter
        .finish_owned_scope(&mut env, Ok(Flow::Continue), Span::default(), true)
        .unwrap();
    assert!(matches!(next, Flow::Continue));
}

#[test]
fn interpreter_expired_loop_exit_empty_break_rejects_at_scope_boundary() {
    assert_expired_loop_exit_rejects_at_scope_boundary(false, false);
}

#[test]
fn interpreter_expired_loop_exit_empty_continue_rejects_at_scope_boundary() {
    assert_expired_loop_exit_rejects_at_scope_boundary(true, false);
}

#[test]
fn interpreter_expired_loop_exit_owned_break_rejects_at_scope_boundary() {
    assert_expired_loop_exit_rejects_at_scope_boundary(false, true);
}

#[test]
fn interpreter_expired_loop_exit_owned_continue_rejects_at_scope_boundary() {
    assert_expired_loop_exit_rejects_at_scope_boundary(true, true);
}

fn assert_single_cleanup_scope_was_popped(env: &mut Env) {
    // Each caller starts with ROOT plus exactly one scope. If finish did not
    // pop even the empty scope, this binding would disappear on the next pop.
    assert!(!env.contains("owned"));
    env.define_owned(
        "root_after_cleanup".into(),
        Value::Int(31),
        false,
        Span::default(),
    )
    .unwrap();
    env.pop_scope();
    assert_eq!(
        env.get("root_after_cleanup", Span::default()).unwrap(),
        Value::Int(31)
    );
}

fn cleanup_scope_with_optional_owned_value(has_owned_value: bool) -> Env {
    let mut env = Env::new();
    env.push_scope();
    if has_owned_value {
        env.define_owned(
            "owned".into(),
            Value::String("bounded owned cleanup fixture".into()),
            false,
            Span::default(),
        )
        .unwrap();
    }
    env
}

fn assert_loop_exit_uses_minimum_context(carried_is_expired: bool) {
    for is_continue in [false, true] {
        for has_owned_value in [false, true] {
            let _execution = ExecutionTerminationGuard::enter();
            let mut interpreter = Interpreter::new();
            let mut env = cleanup_scope_with_optional_owned_value(has_owned_value);
            let now = Instant::now();
            let expired = now.checked_sub(Duration::from_millis(1)).unwrap();
            let future = now.checked_add(Duration::from_secs(1)).unwrap();
            let carried = RuntimeTermination {
                reason: TerminationReason::Cancelled,
                cleanup_deadline: if carried_is_expired { expired } else { future },
            };
            let ambient = RuntimeTermination {
                reason: TerminationReason::TimedOut,
                cleanup_deadline: if carried_is_expired { future } else { expired },
            };
            let guard = CleanupGuard::enter(ambient);
            let flow = if is_continue {
                Flow::LoopContinue(Some(carried))
            } else {
                Flow::Break(Some(carried))
            };
            let result = interpreter.finish_owned_scope(&mut env, Ok(flow), Span::default(), true);
            assert_single_cleanup_scope_was_popped(&mut env);
            let error = result.err().expect("minimum cleanup deadline is expired");
            assert_eq!(error.domain.as_deref(), Some("task"));
            assert_eq!(error.code.as_deref(), Some("shutdown_timeout"));
            assert_eq!(
                error.scope_cleanup(),
                Some(RuntimeTermination {
                    reason: carried.reason,
                    cleanup_deadline: expired,
                })
            );
            assert!(error.runtime_termination().is_none());
            assert!(interpreter.termination.is_none());
            assert!(current_task_cancellation().is_none());
            assert!(crate::runtime::task::current_execution_termination().is_none());
            // A nested cleanup guard may tighten its parent's deadline, but
            // must neither replace the parent's reason nor remove its scope.
            let remaining = current_cleanup_context().unwrap();
            assert_eq!(remaining.reason, ambient.reason);
            assert!(remaining.cleanup_deadline <= ambient.cleanup_deadline);
            drop(guard);
            assert!(current_cleanup_context().is_none());
            env.push_scope();
            let next = interpreter
                .finish_owned_scope(&mut env, Ok(Flow::Continue), Span::default(), true)
                .unwrap();
            assert!(matches!(next, Flow::Continue));
        }
    }
}

#[test]
fn interpreter_loop_exit_future_carried_uses_expired_ambient_deadline() {
    assert_loop_exit_uses_minimum_context(false);
}

#[test]
fn interpreter_loop_exit_expired_carried_is_not_extended_by_future_ambient() {
    assert_loop_exit_uses_minimum_context(true);
}

#[test]
fn interpreter_loop_exit_runtime_termination_keeps_primary_and_tightens_deadline() {
    for reason in [TerminationReason::Cancelled, TerminationReason::TimedOut] {
        for has_owned_value in [false, true] {
            {
                let _execution = ExecutionTerminationGuard::enter();
                let mut interpreter = Interpreter::new();
                let mut env = cleanup_scope_with_optional_owned_value(has_owned_value);
                let now = Instant::now();
                let expired = now.checked_sub(Duration::from_millis(1)).unwrap();
                let original = RuntimeTermination {
                    reason,
                    cleanup_deadline: now.checked_add(Duration::from_secs(1)).unwrap(),
                };
                let ambient = RuntimeTermination {
                    reason: if reason == TerminationReason::Cancelled {
                        TerminationReason::TimedOut
                    } else {
                        TerminationReason::Cancelled
                    },
                    cleanup_deadline: expired,
                };
                let mut primary = KuError::termination(original, Span::default());
                primary.message = "original typed runtime termination fixture".into();
                let expected_code = primary.code.clone();
                let expected_message = primary.message.clone();
                let guard = CleanupGuard::enter(ambient);
                let result: KuResult<Flow> =
                    interpreter.finish_owned_scope(&mut env, Err(primary), Span::default(), true);
                assert_single_cleanup_scope_was_popped(&mut env);
                let error = result
                    .err()
                    .expect("runtime termination cannot become success");
                assert_eq!(error.domain.as_deref(), Some("task"));
                assert_eq!(error.code, expected_code);
                assert_eq!(error.message, expected_message);
                let expected = RuntimeTermination {
                    reason,
                    cleanup_deadline: expired,
                };
                assert_eq!(error.runtime_termination(), Some(expected));
                assert_eq!(error.scope_cleanup(), Some(expected));
                assert_eq!(interpreter.termination.unwrap().reason, reason);
                assert!(current_task_cancellation().is_none());
                assert_eq!(current_cleanup_context().unwrap().reason, ambient.reason);
                drop(guard);
                assert!(current_cleanup_context().is_none());
                // Real termination remains latched inside this execution.
                assert_eq!(
                    crate::runtime::task::current_execution_termination()
                        .unwrap()
                        .reason,
                    reason
                );
            }
            // The execution boundary, not a local cleanup error, restores TLS.
            assert!(crate::runtime::task::current_execution_termination().is_none());
        }
    }
}

#[test]
fn interpreter_loop_exit_independent_empty_and_owned_scopes_need_no_context() {
    let _execution = ExecutionTerminationGuard::enter();
    let mut interpreter = Interpreter::new();
    for has_owned_value in [false, true] {
        let mut env = cleanup_scope_with_optional_owned_value(has_owned_value);
        let flow = interpreter
            .finish_owned_scope(&mut env, Ok(Flow::Continue), Span::default(), true)
            .unwrap();
        assert!(matches!(flow, Flow::Continue));
        assert_single_cleanup_scope_was_popped(&mut env);
        assert!(interpreter.termination.is_none());
        assert!(current_cleanup_context().is_none());
        assert!(current_task_cancellation().is_none());
        assert!(crate::runtime::task::current_execution_termination().is_none());
    }
}

#[test]
fn interpreter_scope_exit_discard_drop_tightens_error_deadline_under_guard() {
    use std::cell::Cell;

    struct DropWitness<'a> {
        cleanup: RuntimeTermination,
        tighter: RuntimeTermination,
        collections: &'a Cell<usize>,
        drops: &'a Cell<usize>,
        observed: &'a Cell<[Option<RuntimeTermination>; 3]>,
    }

    impl cancellation::ScopeExit for DropWitness<'_> {
        fn cleanup_context(&self) -> Option<RuntimeTermination> {
            Some(self.cleanup)
        }

        fn remember_cleanup(&mut self, context: RuntimeTermination) {
            self.cleanup.cleanup_deadline =
                self.cleanup.cleanup_deadline.min(context.cleanup_deadline);
        }

        fn collect_tasks(
            &self,
            _tasks: &mut Vec<crate::runtime::task::TaskHandle>,
            _span: Span,
        ) -> KuResult<()> {
            self.collections.set(self.collections.get() + 1);
            Ok(())
        }
    }

    impl Drop for DropWitness<'_> {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
            let before = current_cleanup_context();
            let guard = CleanupGuard::enter(self.tighter);
            let during = current_cleanup_context();
            drop(guard);
            self.observed
                .set([before, during, current_cleanup_context()]);
        }
    }

    // A test-only outgoing value exercises real discard_flow, not a physical
    // caller Env/lease teardown or an elapsed HTTP/native timeout.
    let _execution = ExecutionTerminationGuard::enter();
    let mut interpreter = Interpreter::new();
    let mut env = cleanup_scope_with_optional_owned_value(true);
    let now = Instant::now();
    let original = RuntimeTermination {
        reason: TerminationReason::Cancelled,
        cleanup_deadline: now.checked_sub(Duration::from_millis(1)).unwrap(),
    };
    let tighter = RuntimeTermination {
        reason: TerminationReason::TimedOut,
        cleanup_deadline: now.checked_sub(Duration::from_millis(2)).unwrap(),
    };
    let expected = RuntimeTermination {
        reason: original.reason,
        cleanup_deadline: tighter.cleanup_deadline,
    };
    let collections = Cell::new(0);
    let drops = Cell::new(0);
    let observed = Cell::new([None; 3]);
    let witness = DropWitness {
        cleanup: original,
        tighter,
        collections: &collections,
        drops: &drops,
        observed: &observed,
    };
    assert!(interpreter.termination.is_none());
    assert!(current_cleanup_context().is_none());
    assert!(current_task_cancellation().is_none());
    assert!(crate::runtime::task::current_execution_termination().is_none());
    let error = interpreter
        .finish_owned_scope(&mut env, Ok(witness), Span::default(), true)
        .err()
        .expect("expired cleanup must discard the outgoing witness");
    assert_single_cleanup_scope_was_popped(&mut env);
    assert_eq!(
        collections.get(),
        1,
        "discard_flow must inspect the payload"
    );
    assert_eq!(drops.get(), 1);
    assert_eq!(
        observed.get(),
        [Some(original), Some(expected), Some(expected)]
    );
    assert_eq!(error.domain.as_deref(), Some("task"));
    assert_eq!(error.code.as_deref(), Some("shutdown_timeout"));
    assert_eq!(error.scope_cleanup(), Some(expected));
    assert!(error.runtime_termination().is_none());
    assert!(interpreter.termination.is_none());
    assert!(current_cleanup_context().is_none());
    assert!(current_task_cancellation().is_none());
    assert!(crate::runtime::task::current_execution_termination().is_none());

    env.push_scope();
    let next = interpreter
        .finish_owned_scope(&mut env, Ok(Flow::Continue), Span::default(), true)
        .unwrap();
    assert!(matches!(next, Flow::Continue));
    assert_eq!(drops.get(), 1);
    assert!(interpreter.termination.is_none());
    assert!(current_cleanup_context().is_none());
    assert!(current_task_cancellation().is_none());
    assert!(crate::runtime::task::current_execution_termination().is_none());
}

#[derive(Clone, Copy)]
enum ViewTaskCallPath {
    Direct,
    FunctionValue,
    FunctionExpression,
    NamedExpression,
}

enum PreparedViewTaskCall {
    Direct(FnDecl),
    FunctionValue(Value),
    FunctionExpression(Expr),
    NamedExpression(Expr),
}

fn assert_view_task_settlement(path: ViewTaskCallPath, borrowed_from_caller: bool) {
    const CHILD: &str = "KU_DIRECT_VIEW_TASK_LEASE_ORACLE_CHILD";
    let test_name = match (path, borrowed_from_caller) {
        (ViewTaskCallPath::Direct, false) => "runtime::interpreter::task_cleanup_tests::interpreter_direct_temporary_view_task_cannot_outlive_successful_call",
        (ViewTaskCallPath::Direct, true) => "runtime::interpreter::task_cleanup_tests::interpreter_direct_caller_owned_view_task_remains_with_caller",
        (ViewTaskCallPath::FunctionValue, false) => "runtime::interpreter::task_cleanup_tests::interpreter_function_value_temporary_view_task_cannot_outlive_successful_call",
        (ViewTaskCallPath::FunctionValue, true) => "runtime::interpreter::task_cleanup_tests::interpreter_function_value_caller_owned_view_task_remains_with_caller",
        (ViewTaskCallPath::FunctionExpression, false) => "runtime::interpreter::task_cleanup_tests::interpreter_function_expression_temporary_view_task_cannot_outlive_successful_call",
        (ViewTaskCallPath::FunctionExpression, true) => "runtime::interpreter::task_cleanup_tests::interpreter_function_expression_caller_owned_view_task_remains_with_caller",
        (ViewTaskCallPath::NamedExpression, false) => "runtime::interpreter::task_cleanup_tests::interpreter_named_expression_temporary_view_task_cannot_outlive_successful_call",
        (ViewTaskCallPath::NamedExpression, true) => "runtime::interpreter::task_cleanup_tests::interpreter_named_expression_caller_owned_view_task_remains_with_caller",
    };
    if std::env::var_os(CHILD).is_none() {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", test_name, "--nocapture"])
            .env(CHILD, "1");
        let output = finally_bounded_process::run_bounded(
            &mut command,
            Duration::from_secs(15),
            finally_bounded_process::OutputLimits::new(64 * 1024, 128 * 1024),
        )
        .expect("temporary view ownership test must be process bounded");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let _execution = ExecutionTerminationGuard::enter();
    let span = Span::default();
    // All parser/checker, AST-shape, and callable preflight is before spawn.
    // Direct paths inject the argument at their internal boundary. Expression
    // paths evaluate the parsed call, but do not execute Probe/main/Child.
    let source = match (path, borrowed_from_caller) {
        (ViewTaskCallPath::Direct, _) => "fn Inspect<T>(&value: T): int { return 7 } fn main() {}",
        (ViewTaskCallPath::FunctionValue | ViewTaskCallPath::FunctionExpression, false) => {
            r#"fn Identity<T>(value: T): T { return value }
fn Probe<T>(value: T): int {
    op = fn(&item: T): int { return 7 }
    return op(Identity(value))
}
async fn Child(): null! { return ok(null) }
async fn main(): null! {
    child = Child()
    result = Probe(child)
    return ok(null)
}
"#
        }
        (ViewTaskCallPath::FunctionValue | ViewTaskCallPath::FunctionExpression, true) => {
            r#"fn Identity<T>(value: T): T { return value }
fn Probe<T>(&value: T): int {
    op = fn(&item: T): int { return 7 }
    return op(value)
}
async fn Child(): null! { return ok(null) }
async fn main(): null! {
    child = Child()
    result = Probe(child)
    return ok(null)
}
"#
        }
        (ViewTaskCallPath::NamedExpression, false) => {
            r#"fn Inspect<T>(&value: T): int { return 7 }
fn Identity<T>(value: T): T { return value }
async fn Child(): null! { return ok(null) }
async fn main(): null! {
    child = Child()
    result = Inspect(Identity(child))
    return ok(null)
}
"#
        }
        (ViewTaskCallPath::NamedExpression, true) => {
            r#"fn Inspect<T>(&value: T): int { return 7 }
fn Identity<T>(value: T): T { return value }
async fn Child(): null! { return ok(null) }
async fn main(): null! {
    child = Child()
    result = Inspect(child)
    return ok(null)
}
"#
        }
    };
    let program = Parser::new(Lexer::new(source).tokenize().unwrap())
        .parse_program()
        .unwrap();
    crate::checker::Checker::new().check(&program).unwrap();
    assert!(current_cleanup_context().is_none());
    assert!(current_task_cancellation().is_none());
    assert!(crate::runtime::task::current_execution_termination().is_none());

    let runtime = TaskRuntime::new();
    let mut interpreter = Interpreter::new();
    interpreter.task_runtime = Some(runtime.clone());
    let mut caller = Env::new();
    let mut caller_lease = None;
    let declaration = |name: &str| {
        program
            .items
            .iter()
            .find_map(|item| match item {
                Item::Function(function) if function.name == name => Some(function),
                _ => None,
            })
            .expect("fixture declaration must exist")
    };
    let prepared = match path {
        ViewTaskCallPath::Direct => {
            let function = declaration("Inspect");
            assert_eq!(function.params.len(), 1);
            assert_eq!(function.params[0].mode, ParamMode::View);
            PreparedViewTaskCall::Direct(function.clone())
        }
        ViewTaskCallPath::FunctionValue | ViewTaskCallPath::FunctionExpression => {
            let literal = declaration("Probe")
                .body
                .iter()
                .find_map(|stmt| match stmt {
                    Stmt::Assign { name, value, .. } if name == "op" => Some(value),
                    _ => None,
                })
                .expect("Probe must contain the op literal");
            let ExprKind::Function { params, body, .. } = &literal.kind else {
                panic!("op must be a closure literal, not a generic function value");
            };
            assert_eq!(params.len(), 1);
            assert_eq!(params[0].mode, ParamMode::View);
            assert!(closure_capture_names(params, body).unwrap().is_empty());
            let callable = interpreter.eval(literal, &mut caller, 0).unwrap();
            let Value::Function {
                params,
                param_modes,
                is_async,
                ..
            } = &callable
            else {
                panic!("op literal must evaluate to a function value");
            };
            assert_eq!(params.len(), 1);
            assert_eq!(param_modes, &[ParamMode::View]);
            assert!(!*is_async);
            if matches!(path, ViewTaskCallPath::FunctionExpression) {
                let expression = declaration("Probe")
                    .body
                    .iter()
                    .find_map(|stmt| match stmt {
                        Stmt::Return {
                            value: Some(value), ..
                        } => Some(value),
                        _ => None,
                    })
                    .expect("Probe must return the op call expression");
                let ExprKind::Call { callee, args } = &expression.kind else {
                    panic!("Probe return must be the op call expression");
                };
                assert!(matches!(&callee.kind, ExprKind::Variable(name) if name == "op"));
                assert_eq!(args.len(), 1);
                if borrowed_from_caller {
                    assert!(matches!(&args[0].kind, ExprKind::Variable(name) if name == "value"));
                } else {
                    let ExprKind::Call { callee, args } = &args[0].kind else {
                        panic!("temporary argument must be Identity(value)");
                    };
                    assert!(matches!(&callee.kind, ExprKind::Variable(name) if name == "Identity"));
                    assert_eq!(args.len(), 1);
                    assert!(matches!(&args[0].kind, ExprKind::Variable(name) if name == "value"));
                }
                caller
                    .define_owned("op".into(), callable, false, literal.span)
                    .unwrap();
                interpreter
                    .functions
                    .insert("Identity".into(), declaration("Identity").clone());
                PreparedViewTaskCall::FunctionExpression(expression.clone())
            } else {
                PreparedViewTaskCall::FunctionValue(callable)
            }
        }
        ViewTaskCallPath::NamedExpression => {
            let expression = declaration("main")
                .body
                .iter()
                .find_map(|stmt| match stmt {
                    Stmt::Assign { name, value, .. } if name == "result" => Some(value),
                    _ => None,
                })
                .expect("main must contain the result initializer");
            let ExprKind::Call { callee, args } = &expression.kind else {
                panic!("result must be the Inspect call expression");
            };
            assert!(matches!(&callee.kind, ExprKind::Variable(name) if name == "Inspect"));
            assert_eq!(args.len(), 1);
            if borrowed_from_caller {
                assert!(matches!(&args[0].kind, ExprKind::Variable(name) if name == "child"));
            } else {
                let ExprKind::Call { callee, args } = &args[0].kind else {
                    panic!("temporary argument must be Identity(child)");
                };
                assert!(matches!(&callee.kind, ExprKind::Variable(name) if name == "Identity"));
                assert_eq!(args.len(), 1);
                assert!(matches!(&args[0].kind, ExprKind::Variable(name) if name == "child"));
            }
            for name in ["Inspect", "Identity"] {
                interpreter
                    .functions
                    .insert(name.into(), declaration(name).clone());
            }
            interpreter.async_execution = true;
            PreparedViewTaskCall::NamedExpression(expression.clone())
        }
    };
    // New source fixtures declare Child(): null!; its injected Task has that
    // payload shape. The original direct fixture keeps its original Null.
    let result_payload = !matches!(path, ViewTaskCallPath::Direct);
    let source_name = if matches!(path, ViewTaskCallPath::FunctionExpression) {
        "value"
    } else {
        "child"
    };
    let wait = Duration::from_secs(3);
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let (observed_tx, observed_rx) = mpsc::sync_channel(1);
    let child = runtime
        .spawn(move || {
            ready_tx
                .send(())
                .map_err(|_| KuError::runtime("fixture readiness receiver disappeared", span))?;
            // Completion is impossible until explicitly released. The finite
            // receive is failure containment, not sleep or a timing assertion.
            let released = release_rx.recv_timeout(wait).is_ok();
            let context = current_task_cancellation();
            let _ = observed_tx.send((released, context));
            if released {
                Ok(if result_payload {
                    Value::Result {
                        ok: true,
                        value: Box::new(Value::Null),
                    }
                } else {
                    Value::Null
                })
            } else {
                Err(KuError::runtime("fixture child release timed out", span))
            }
        })
        .unwrap();
    // No TaskHandle clone/Env capture is retained as an observer. Direct
    // negatives transfer into a callee lease; expression negatives let
    // Identity move the caller slot into the real outer argument lease.
    let ready = ready_rx.recv_timeout(wait);
    let reached_barrier = ready.is_ok();
    let outcome = (|| -> KuResult<Option<i64>> {
        ready.map_err(|_| KuError::runtime("fixture child did not reach barrier", span))?;
        let arguments = if matches!(
            &prepared,
            PreparedViewTaskCall::NamedExpression(_) | PreparedViewTaskCall::FunctionExpression(_)
        ) {
            caller.define_owned(source_name.into(), Value::Task(child), false, span)?;
            Vec::new()
        } else if borrowed_from_caller {
            caller.define_owned("child".into(), Value::Task(child), false, span)?;
            let (view, lease) = caller.borrow("child", span)?;
            caller_lease = lease;
            vec![view]
        } else {
            vec![Value::Task(child)]
        };
        let result = match prepared {
            PreparedViewTaskCall::Direct(function) => {
                interpreter.call_function_direct(function, arguments, span, 0)
            }
            PreparedViewTaskCall::FunctionValue(callable) => callable.with_read(span, |callable| {
                let Value::Function {
                    params,
                    param_modes,
                    body,
                    captures,
                    self_name,
                    is_async,
                } = callable
                else {
                    return Err(KuError::runtime(
                        "fixture callable changed after preflight",
                        span,
                    ));
                };
                interpreter.call_function_value(FunctionValueCall {
                    params,
                    param_modes,
                    body,
                    captures,
                    self_name,
                    is_async: *is_async,
                    args: arguments,
                    span,
                    depth: 0,
                })
            }),
            PreparedViewTaskCall::NamedExpression(expression)
            | PreparedViewTaskCall::FunctionExpression(expression) => {
                interpreter.eval(&expression, &mut caller, 0)
            }
        };
        result.map(|value| match value {
            Value::Int(value) => Some(value),
            _ => None,
        })
    })();
    let local_state_restored = interpreter.termination.is_none()
        && current_cleanup_context().is_none()
        && current_task_cancellation().is_none()
        && crate::runtime::task::current_execution_termination().is_none();
    drop(caller_lease);
    let call_bookkeeping_restored =
        interpreter.caller_owners.is_empty() && interpreter.pending_fail.is_none();
    let source_slot_is_task = matches!(
        path,
        ViewTaskCallPath::NamedExpression | ViewTaskCallPath::FunctionExpression
    )
    .then(|| {
        caller.with_value(source_name, span, |value| {
            Ok(match value {
                Value::Task(_) => Some(true),
                Value::Null => Some(false),
                _ => None,
            })
        })
    });
    // On ordinary KuResult paths, release and close before semantic asserts,
    // including the expected RED where the call incorrectly returned Ok(7).
    // A Rust panic instead relies on the bounded subprocess/child wait; it
    // does not prove that this explicit release/drain sequence was reached.
    // Observe child cancellation before cancel_all_and_wait can introduce it.
    let released = release_tx.send(());
    let observed = observed_rx.recv_timeout(wait);
    let drained = runtime.cancel_all_and_wait(Duration::from_secs(2));
    let snapshot = runtime.snapshot();
    drop(caller);
    drop(interpreter);
    drop(runtime);

    assert!(reached_barrier, "fixture did not start the child");
    assert!(released.is_ok(), "fixture release receiver disappeared");
    let (explicit_release, child_context) = observed.expect("fixture child did not report release");
    assert!(
        explicit_release,
        "fixture's own wait expired before release"
    );
    drained.expect("fixture runtime did not drain after explicit release");
    let snapshot = snapshot.unwrap();
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(snapshot.running_blocking_jobs, 0);
    assert!(
        local_state_restored,
        "local cleanup latched parent cancellation"
    );
    assert!(
        call_bookkeeping_restored,
        "call observers or pending fail leaked"
    );
    if let Some(slot) = source_slot_is_task {
        assert_eq!(slot.unwrap(), Some(borrowed_from_caller));
    }
    if borrowed_from_caller {
        assert_eq!(outcome.unwrap(), Some(7));
        assert!(
            child_context.is_none(),
            "callee cancelled its caller's borrowed Task"
        );
    } else {
        let error =
            outcome.expect_err("unfinished temporary Task owner cannot escape as call success");
        assert_eq!(error.domain.as_deref(), Some("task"));
        assert_eq!(error.code.as_deref(), Some("shutdown_timeout"));
        assert!(error.runtime_termination().is_none());
        let context = error
            .scope_cleanup()
            .expect("local cleanup deadline was lost");
        assert_eq!(context.reason, TerminationReason::Cancelled);
        assert_eq!(child_context, Some(context));
    }
}

#[test]
fn interpreter_direct_temporary_view_task_cannot_outlive_successful_call() {
    assert_view_task_settlement(ViewTaskCallPath::Direct, false);
}

#[test]
fn interpreter_direct_caller_owned_view_task_remains_with_caller() {
    assert_view_task_settlement(ViewTaskCallPath::Direct, true);
}

#[test]
fn interpreter_function_value_temporary_view_task_cannot_outlive_successful_call() {
    assert_view_task_settlement(ViewTaskCallPath::FunctionValue, false);
}

#[test]
fn interpreter_function_value_caller_owned_view_task_remains_with_caller() {
    assert_view_task_settlement(ViewTaskCallPath::FunctionValue, true);
}

#[test]
fn interpreter_function_expression_temporary_view_task_cannot_outlive_successful_call() {
    assert_view_task_settlement(ViewTaskCallPath::FunctionExpression, false);
}

#[test]
fn interpreter_function_expression_caller_owned_view_task_remains_with_caller() {
    assert_view_task_settlement(ViewTaskCallPath::FunctionExpression, true);
}

#[test]
fn interpreter_named_expression_temporary_view_task_cannot_outlive_successful_call() {
    assert_view_task_settlement(ViewTaskCallPath::NamedExpression, false);
}

#[test]
fn interpreter_named_expression_caller_owned_view_task_remains_with_caller() {
    assert_view_task_settlement(ViewTaskCallPath::NamedExpression, true);
}

#[derive(Clone, Copy)]
enum TemporaryCallBudgetExit {
    ReturnTask,
    Fail,
    Fallthrough,
}

fn assert_temporary_call_budget_exit(exit: TemporaryCallBudgetExit) {
    const CHILD: &str = "KU_TEMPORARY_CALL_BUDGET_CHILD";
    let test_name = match exit {
        TemporaryCallBudgetExit::ReturnTask => "runtime::interpreter::task_cleanup_tests::interpreter_temporary_call_returned_task_keeps_owner_and_deadline",
        TemporaryCallBudgetExit::Fail => "runtime::interpreter::task_cleanup_tests::interpreter_temporary_call_fail_keeps_deadline_into_outer_lease",
        TemporaryCallBudgetExit::Fallthrough => "runtime::interpreter::task_cleanup_tests::interpreter_temporary_call_fallthrough_keeps_deadline_into_outer_lease",
    };
    if std::env::var_os(CHILD).is_none() {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", test_name, "--nocapture"])
            .env(CHILD, "1");
        let output = finally_bounded_process::run_bounded(
            &mut command,
            Duration::from_secs(15),
            finally_bounded_process::OutputLimits::new(64 * 1024, 128 * 1024),
        )
        .expect("temporary call budget test must be process bounded");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let _execution = ExecutionTerminationGuard::enter();
    let span = Span::default();
    let source = match exit {
        TemporaryCallBudgetExit::ReturnTask => {
            "fn Complete<T, R>(&temporary: T, value: R): R { return value } fn main() {}"
        }
        TemporaryCallBudgetExit::Fail => {
            r#"fn Complete<T>(&temporary: T): int! {
                fail { domain: "fixture", code: "expected", message: "budget" }
            }
            fn main() {}"#
        }
        TemporaryCallBudgetExit::Fallthrough => "fn Complete<T>(&temporary: T) {} fn main() {}",
    };
    // Parser/checker/shape assertions happen before any Task exists.
    let program = Parser::new(Lexer::new(source).tokenize().unwrap())
        .parse_program()
        .unwrap();
    crate::checker::Checker::new().check(&program).unwrap();
    let Item::Function(function) = program.items.into_iter().next().unwrap() else {
        panic!("fixture must begin with Complete");
    };
    assert_eq!(function.params[0].mode, ParamMode::View);
    assert_eq!(
        function.params.len(),
        if matches!(exit, TemporaryCallBudgetExit::ReturnTask) {
            2
        } else {
            1
        }
    );
    assert!(current_cleanup_context().is_none());
    assert!(current_task_cancellation().is_none());
    assert!(crate::runtime::task::current_execution_termination().is_none());

    let runtime = TaskRuntime::new();
    let mut interpreter = Interpreter::new();
    interpreter.task_runtime = Some(runtime.clone());
    let mut caller = Env::new();
    let wait = Duration::from_secs(3);
    let mut returned_observer = None;
    let mut returned_id = None;
    let mut callee_context = None;
    let mut outer_context = None;
    let mut outer_lease_count = 0;
    let mut callee_restored = false;
    let outcome = (|| -> KuResult<Value> {
        // Distinct sole owners. These children need no artificial delay: the
        // returned Flow exposes the exact minted deadline deterministically.
        let local = runtime.spawn(|| Ok(Value::Null))?;
        let outer = runtime.spawn(|| Ok(Value::Null))?;
        let mut arguments = vec![Value::Task(local)];
        if matches!(exit, TemporaryCallBudgetExit::ReturnTask) {
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let (release_tx, release_rx) = mpsc::sync_channel(1);
            let (observed_tx, observed_rx) = mpsc::sync_channel(1);
            let returned = runtime.spawn(move || {
                ready_tx.send(()).map_err(|_| {
                    KuError::runtime("returned Task readiness receiver disappeared", span)
                })?;
                let released = release_rx.recv_timeout(wait).is_ok();
                // Sample before the parent's final runtime drain can cancel
                // anything. No TaskHandle clone is retained as an observer.
                let _ = observed_tx.send((released, current_task_cancellation()));
                if released {
                    Ok(Value::Null)
                } else {
                    Err(KuError::runtime("returned Task release timed out", span))
                }
            })?;
            returned_id = Some(returned.id());
            returned_observer = Some((release_tx, observed_rx));
            arguments.push(Value::Task(returned));
            ready_rx
                .recv_timeout(wait)
                .map_err(|_| KuError::runtime("returned Task did not reach its barrier", span))?;
        }
        // This intentionally composes two real internal boundaries. The outer
        // lease is synthetic; it does not claim source-expression coverage.
        let (outer_view, outer_lease) = BorrowLease::temporary(Value::Task(outer));
        drop(outer_view);
        let leases: Vec<_> = outer_lease.into_iter().collect();
        outer_lease_count = leases.len();
        let flow = interpreter.call_function_direct_flow(function, arguments, span, 0);
        callee_context = flow
            .as_ref()
            .ok()
            .and_then(cancellation::ScopeExit::cleanup_context);
        callee_restored = interpreter.termination.is_none()
            && current_cleanup_context().is_none()
            && current_task_cancellation().is_none()
            && crate::runtime::task::current_execution_termination().is_none();
        // Preserve Flow here; projecting to Value before this boundary would
        // erase D and allow this independent outer owner to mint a new budget.
        let flow =
            interpreter.finish_call_arguments(&mut caller, leases, Vec::new(), flow, span)?;
        outer_context = cancellation::ScopeExit::cleanup_context(&flow);
        flow.into_call_value(span, "temporary budget fixture")
    })();
    let local_state_restored = interpreter.termination.is_none()
        && current_cleanup_context().is_none()
        && current_task_cancellation().is_none()
        && crate::runtime::task::current_execution_termination().is_none();
    let bookkeeping_restored = interpreter.call_depth == 0
        && interpreter.caller_owners.is_empty()
        && interpreter.pending_fail.is_none();

    // No semantic assertion while a child can remain blocked. Even an ordinary
    // failing result releases/drains first. A Rust panic instead relies on the
    // finite child wait and outer subprocess bound, not an imaginary drain.
    let returned_observation = returned_observer.map(|(release, observed)| {
        let released = release.send(());
        let observed = observed.recv_timeout(wait);
        (released, observed)
    });
    let returned_payload = match &outcome {
        Ok(Value::Task(task)) => Some(task.await_result()),
        _ => None,
    };
    let correct_value = match (&exit, &outcome) {
        (TemporaryCallBudgetExit::ReturnTask, Ok(Value::Task(task))) => {
            Some(task.id()) == returned_id
        }
        (TemporaryCallBudgetExit::Fail, Ok(Value::Result { ok: false, .. })) => true,
        (TemporaryCallBudgetExit::Fallthrough, Ok(Value::Null)) => true,
        _ => false,
    };
    // Consume/drop the returned owner only after the explicit release and
    // observation. Keep only the error and scalar/context receipts for asserts.
    let call_error = outcome.err();
    let drained = runtime.cancel_all_and_wait(Duration::from_secs(2));
    let snapshot = runtime.snapshot();
    drop(caller);
    drop(interpreter);
    drop(runtime);

    drained.expect("budget fixture runtime did not drain");
    let snapshot = snapshot.unwrap();
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(snapshot.running_blocking_jobs, 0);
    assert!(call_error.is_none(), "call failed: {call_error:?}");
    assert!(
        correct_value,
        "call changed the successful value/Result shape"
    );
    assert_eq!(outer_lease_count, 1);
    let original = callee_context.expect("callee temporary cleanup lost its Flow deadline");
    assert_eq!(original.reason, TerminationReason::Cancelled);
    assert_eq!(
        outer_context,
        Some(original),
        "outer temporary lease replaced the carried deadline"
    );
    assert!(
        callee_restored && local_state_restored,
        "local cleanup cancelled its parent"
    );
    assert!(bookkeeping_restored, "call bookkeeping was not restored");
    if matches!(exit, TemporaryCallBudgetExit::ReturnTask) {
        let (released, observed) = returned_observation.expect("missing returned Task observer");
        assert!(
            released.is_ok(),
            "returned Task release receiver disappeared"
        );
        let (explicit_release, cancellation) = observed.expect("returned Task did not report");
        assert!(explicit_release, "returned Task exhausted its fixture wait");
        assert!(
            cancellation.is_none(),
            "cleanup cancelled a successfully returned owner"
        );
        assert_eq!(returned_payload.unwrap().unwrap(), Value::Null);
    } else {
        assert!(returned_observation.is_none());
        assert!(returned_payload.is_none());
    }
}

#[test]
fn interpreter_temporary_call_returned_task_keeps_owner_and_deadline() {
    assert_temporary_call_budget_exit(TemporaryCallBudgetExit::ReturnTask);
}

#[test]
fn interpreter_temporary_call_fail_keeps_deadline_into_outer_lease() {
    assert_temporary_call_budget_exit(TemporaryCallBudgetExit::Fail);
}

#[test]
fn interpreter_temporary_call_fallthrough_keeps_deadline_into_outer_lease() {
    assert_temporary_call_budget_exit(TemporaryCallBudgetExit::Fallthrough);
}

// These complete fixtures are parser/checked before a Task exists. Markers
// distinguish reaching the second argument from entering the named target body.
// main/Probe/Child are checker fixtures, never executed by this Task oracle.
const INTERRUPTED_ARGUMENT_ORDINARY_SOURCE: &str = r#"
fn Identity<T>(value: T): T { return value }
fn Zero(): int {
    println("KU_ARGUMENT_INTERRUPT_SECOND_ENTERED")
    return 0
}
fn Inspect<T>(&item: T, second: int, third: int): int {
    println("KU_ARGUMENT_INTERRUPT_NAMED_BODY_ENTERED")
    return second + third
}

fn NamedOwned<T>(value: T): int {
    tail_calls = 0
    tail = fn(): int { tail_calls = tail_calls + 1 return 9 }
    result = Inspect(Identity(value), 1 / Zero(), tail())
    return result
}

fn NamedBorrowed<T>(&value: T): int {
    tail_calls = 0
    tail = fn(): int { tail_calls = tail_calls + 1 return 9 }
    result = Inspect(value, 1 / Zero(), tail())
    return result
}

fn FirstClassOwned<T>(value: T): int {
    tail_calls = 0
    entered_calls = 0
    tail = fn(): int { tail_calls = tail_calls + 1 return 9 }
    op = fn(&item: T, second: int, third: int): int {
        entered_calls = entered_calls + 1
        return second + third
    }
    result = op(Identity(value), 1 / Zero(), tail())
    return result
}

fn FirstClassBorrowed<T>(&value: T): int {
    tail_calls = 0
    entered_calls = 0
    tail = fn(): int { tail_calls = tail_calls + 1 return 9 }
    op = fn(&item: T, second: int, third: int): int {
        entered_calls = entered_calls + 1
        return second + third
    }
    result = op(value, 1 / Zero(), tail())
    return result
}

async fn Child(): null! { return ok(null) }
async fn main(): null! {
    a = Child()
    unused_a = NamedOwned(a)
    b = Child()
    unused_b = NamedBorrowed(b)
    c = Child()
    unused_c = FirstClassOwned(c)
    d = Child()
    unused_d = FirstClassBorrowed(d)
    return ok(null)
}
"#;

const INTERRUPTED_ARGUMENT_QUESTION_SOURCE: &str = r#"
fn Identity<T>(value: T): T { return value }
fn Trouble(): int! {
    println("KU_ARGUMENT_INTERRUPT_SECOND_ENTERED")
    fail {
        domain: "fixture.arguments",
        code: "late_failure",
        message: "late-" + "argument"
    }
}
fn Inspect<T>(&item: T, second: int, third: int): int {
    println("KU_ARGUMENT_INTERRUPT_NAMED_BODY_ENTERED")
    return second + third
}

fn NamedOwned<T>(value: T): int! {
    tail_calls = 0
    tail = fn(): int { tail_calls = tail_calls + 1 return 9 }
    result = Inspect(Identity(value), Trouble()?, tail())
    return ok(result)
}

fn NamedBorrowed<T>(&value: T): int! {
    tail_calls = 0
    tail = fn(): int { tail_calls = tail_calls + 1 return 9 }
    result = Inspect(value, Trouble()?, tail())
    return ok(result)
}

fn FirstClassOwned<T>(value: T): int! {
    tail_calls = 0
    entered_calls = 0
    tail = fn(): int { tail_calls = tail_calls + 1 return 9 }
    op = fn(&item: T, second: int, third: int): int {
        entered_calls = entered_calls + 1
        return second + third
    }
    result = op(Identity(value), Trouble()?, tail())
    return ok(result)
}

fn FirstClassBorrowed<T>(&value: T): int! {
    tail_calls = 0
    entered_calls = 0
    tail = fn(): int { tail_calls = tail_calls + 1 return 9 }
    op = fn(&item: T, second: int, third: int): int {
        entered_calls = entered_calls + 1
        return second + third
    }
    result = op(value, Trouble()?, tail())
    return ok(result)
}

async fn Child(): null! { return ok(null) }
async fn main(): null! {
    a = Child()
    unused_a = NamedOwned(a)?
    b = Child()
    unused_b = NamedBorrowed(b)?
    c = Child()
    unused_c = FirstClassOwned(c)?
    d = Child()
    unused_d = FirstClassBorrowed(d)?
    return ok(null)
}
"#;

#[derive(Clone, Copy)]
enum InterruptedArgumentKind {
    OrdinaryError,
    QuestionFail,
}

#[derive(Clone, Copy)]
enum InterruptedArgumentStyle {
    Named,
    FirstClass,
}

fn interrupted_fixture_call_args<'a>(expression: &'a Expr, name: &str, count: usize) -> &'a [Expr] {
    let ExprKind::Call { callee, args } = &expression.kind else {
        panic!("interrupted-argument fixture must contain a real call");
    };
    assert!(matches!(&callee.kind, ExprKind::Variable(actual) if actual == name));
    assert_eq!(args.len(), count);
    args
}

fn assert_interrupted_call_arguments(
    kind: InterruptedArgumentKind,
    style: InterruptedArgumentStyle,
    borrowed_from_caller: bool,
) {
    const CHILD: &str = "KU_INTERRUPTED_ARGUMENT_TASK_ORACLE_CHILD";
    const NAMED_BODY_MARKER: &str = "KU_ARGUMENT_INTERRUPT_NAMED_BODY_ENTERED";
    const SECOND_ARGUMENT_MARKER: &str = "KU_ARGUMENT_INTERRUPT_SECOND_ENTERED";
    let test_name = match (kind, style, borrowed_from_caller) {
        (InterruptedArgumentKind::OrdinaryError, InterruptedArgumentStyle::Named, false) => "runtime::interpreter::task_cleanup_tests::interpreter_named_argument_error_temporary_task_is_settled",
        (InterruptedArgumentKind::OrdinaryError, InterruptedArgumentStyle::Named, true) => "runtime::interpreter::task_cleanup_tests::interpreter_named_argument_error_borrowed_task_remains_with_caller",
        (InterruptedArgumentKind::OrdinaryError, InterruptedArgumentStyle::FirstClass, false) => "runtime::interpreter::task_cleanup_tests::interpreter_first_class_argument_error_temporary_task_is_settled",
        (InterruptedArgumentKind::OrdinaryError, InterruptedArgumentStyle::FirstClass, true) => "runtime::interpreter::task_cleanup_tests::interpreter_first_class_argument_error_borrowed_task_remains_with_caller",
        (InterruptedArgumentKind::QuestionFail, InterruptedArgumentStyle::Named, false) => "runtime::interpreter::task_cleanup_tests::interpreter_named_argument_question_temporary_task_discards_pending_fail",
        (InterruptedArgumentKind::QuestionFail, InterruptedArgumentStyle::Named, true) => "runtime::interpreter::task_cleanup_tests::interpreter_named_argument_question_borrowed_task_preserves_pending_fail",
        (InterruptedArgumentKind::QuestionFail, InterruptedArgumentStyle::FirstClass, false) => "runtime::interpreter::task_cleanup_tests::interpreter_first_class_argument_question_temporary_task_discards_pending_fail",
        (InterruptedArgumentKind::QuestionFail, InterruptedArgumentStyle::FirstClass, true) => "runtime::interpreter::task_cleanup_tests::interpreter_first_class_argument_question_borrowed_task_preserves_pending_fail",
    };
    if std::env::var_os(CHILD).is_none() {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", test_name, "--nocapture"])
            .env(CHILD, "1");
        let output = finally_bounded_process::run_bounded(
            &mut command,
            Duration::from_secs(15),
            finally_bounded_process::OutputLimits::new(64 * 1024, 128 * 1024),
        )
        .expect("interrupted argument oracle must be process bounded");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stdout}\n{stderr}");
        assert!(
            stdout.contains("1 passed; 0 failed;"),
            "exact child must run one test, not succeed with zero selected tests:\n{stdout}\n{stderr}"
        );
        // A cleanup timeout can mask an earlier argument error. Require proof
        // that Identity completed and evaluation actually reached argument 2.
        assert_eq!(
            stdout.matches(SECOND_ARGUMENT_MARKER).count(),
            1,
            "second argument did not run exactly once:\n{stdout}\n{stderr}"
        );
        // Named functions have a fresh Env, so they cannot share the local
        // Copy counter. Its first statement instead emits this unique marker.
        // Successful child exit gives a complete raw stdout receipt; --nocapture
        // prevents libtest from hiding it. No body instrumentation changes
        // production code or replaces real named-call dispatch.
        assert!(
            !stdout.contains(NAMED_BODY_MARKER) && !stderr.contains(NAMED_BODY_MARKER),
            "named target body ran after argument interruption:\n{stdout}\n{stderr}"
        );
        return;
    }

    let _execution = ExecutionTerminationGuard::enter();
    let span = Span::default();
    let source = match kind {
        InterruptedArgumentKind::OrdinaryError => INTERRUPTED_ARGUMENT_ORDINARY_SOURCE,
        InterruptedArgumentKind::QuestionFail => INTERRUPTED_ARGUMENT_QUESTION_SOURCE,
    };
    let program = Parser::new(Lexer::new(source).tokenize().unwrap())
        .parse_program()
        .unwrap();
    crate::checker::Checker::new().check(&program).unwrap();
    let declaration = |name: &str| {
        program
            .items
            .iter()
            .find_map(|item| match item {
                Item::Function(function) if function.name == name => Some(function),
                _ => None,
            })
            .expect("interrupted-argument fixture declaration must exist")
    };
    let probe_name = match (style, borrowed_from_caller) {
        (InterruptedArgumentStyle::Named, false) => "NamedOwned",
        (InterruptedArgumentStyle::Named, true) => "NamedBorrowed",
        (InterruptedArgumentStyle::FirstClass, false) => "FirstClassOwned",
        (InterruptedArgumentStyle::FirstClass, true) => "FirstClassBorrowed",
    };
    let probe = declaration(probe_name);
    assert_eq!(probe.params.len(), 1);
    assert_eq!(
        probe.params[0].mode,
        if borrowed_from_caller {
            ParamMode::View
        } else {
            ParamMode::Owned
        }
    );
    let initializer = |name: &str| {
        probe
            .body
            .iter()
            .find_map(|stmt| match stmt {
                Stmt::Assign {
                    name: actual,
                    value,
                    ..
                } if actual == name => Some(value),
                _ => None,
            })
            .expect("interrupted-argument fixture initializer must exist")
    };
    let expression = initializer("result");
    let target = match style {
        InterruptedArgumentStyle::Named => "Inspect",
        InterruptedArgumentStyle::FirstClass => "op",
    };
    let arguments = interrupted_fixture_call_args(expression, target, 3);
    if borrowed_from_caller {
        assert!(matches!(&arguments[0].kind, ExprKind::Variable(name) if name == "value"));
    } else {
        let identity_args = interrupted_fixture_call_args(&arguments[0], "Identity", 1);
        assert!(matches!(&identity_args[0].kind, ExprKind::Variable(name) if name == "value"));
    }
    let interruption_name = match kind {
        InterruptedArgumentKind::OrdinaryError => {
            let ExprKind::Binary { left, op, right } = &arguments[1].kind else {
                panic!("ordinary interruption must be a runtime division");
            };
            assert_eq!(*op, BinaryOp::Divide);
            assert!(matches!(&left.kind, ExprKind::Literal(Literal::Int(1))));
            interrupted_fixture_call_args(right, "Zero", 0);
            "Zero"
        }
        InterruptedArgumentKind::QuestionFail => {
            let ExprKind::TryUnwrap { expr } = &arguments[1].kind else {
                panic!("recoverable interruption must use the actual question operator");
            };
            interrupted_fixture_call_args(expr, "Trouble", 0);
            "Trouble"
        }
    };
    interrupted_fixture_call_args(&arguments[2], "tail", 0);
    let Some(Stmt::Expr { expr: entry, .. }) = declaration(interruption_name).body.first() else {
        panic!("second argument entry marker must be its first statement");
    };
    let marker_args = interrupted_fixture_call_args(entry, "println", 1);
    assert!(
        matches!(&marker_args[0].kind, ExprKind::Literal(Literal::String(text)) if text == SECOND_ARGUMENT_MARKER)
    );
    let inspect = declaration("Inspect");
    assert_eq!(inspect.params.len(), 3);
    assert_eq!(inspect.params[0].mode, ParamMode::View);
    let Some(Stmt::Expr { expr: entry, .. }) = inspect.body.first() else {
        panic!("named body-entry marker must be its first statement");
    };
    let marker_args = interrupted_fixture_call_args(entry, "println", 1);
    assert!(
        matches!(&marker_args[0].kind, ExprKind::Literal(Literal::String(text)) if text == NAMED_BODY_MARKER)
    );

    let mut interpreter = Interpreter::new();
    interpreter.async_execution = true;
    for name in ["Identity", "Inspect", interruption_name] {
        interpreter
            .functions
            .insert(name.into(), declaration(name).clone());
    }
    let mut caller = Env::new();
    for counter in ["tail_calls", "entered_calls"] {
        caller
            .define_owned(counter.into(), Value::Int(0), true, span)
            .unwrap();
    }
    // Only Copy counters are captured. At this point no Task exists, and no
    // TaskHandle/Env clone is retained as an observer.
    for (name, capture) in [("tail", "tail_calls"), ("op", "entered_calls")] {
        if name == "op" && matches!(style, InterruptedArgumentStyle::Named) {
            continue;
        }
        let literal = initializer(name);
        let ExprKind::Function { params, body, .. } = &literal.kind else {
            panic!("fixture callback must be the checked closure literal");
        };
        let captures = closure_capture_names(params, body).unwrap();
        assert_eq!(captures.len(), 1);
        assert!(captures.contains(capture));
        if name == "op" {
            assert_eq!(params.len(), 3);
            assert_eq!(params[0].mode, ParamMode::View);
            assert_eq!(params[1].mode, ParamMode::Owned);
            assert_eq!(params[2].mode, ParamMode::Owned);
        } else {
            assert!(params.is_empty());
        }
        let callable = interpreter.eval(literal, &mut caller, 0).unwrap();
        let Value::Function {
            param_modes,
            is_async,
            ..
        } = &callable
        else {
            panic!("fixture callback must evaluate to a real function value");
        };
        assert!(!*is_async);
        if name == "op" {
            assert_eq!(
                param_modes,
                &[ParamMode::View, ParamMode::Owned, ParamMode::Owned]
            );
        } else {
            assert!(param_modes.is_empty());
        }
        caller
            .define_owned(name.into(), callable, false, literal.span)
            .unwrap();
    }
    // Prove the actual captured counter cells are writable before a Task
    // exists. Otherwise an immutable-counter error could be hidden by the
    // later expected cleanup timeout and leave a false zero observation.
    let tail_probe = interpreter.eval(&arguments[2], &mut caller, 0).unwrap();
    assert!(matches!(tail_probe, Value::Int(9)));
    assert!(
        caller
            .with_value("tail_calls", span, |value| {
                Ok(matches!(value, Value::Int(1)))
            })
            .unwrap(),
        "real tail closure did not increment the observed counter"
    );
    if matches!(style, InterruptedArgumentStyle::FirstClass) {
        let ExprKind::Call { callee, .. } = &expression.kind else {
            unreachable!("call shape was checked before closure setup");
        };
        let callable = interpreter.eval(callee, &mut caller, 0).unwrap();
        let entry_probe = callable
            .with_read(span, |callable| {
                let Value::Function {
                    params,
                    param_modes,
                    body,
                    captures,
                    self_name,
                    is_async,
                } = callable
                else {
                    return Err(KuError::runtime("fixture op is not a function", span));
                };
                // Invoke the same real parsed closure with harmless Copy
                // arguments through the existing internal call boundary.
                // This is only counter liveness preflight: the actual Task
                // oracle below still evals its original parsed expression.
                interpreter.call_function_value(FunctionValueCall {
                    params,
                    param_modes,
                    body,
                    captures,
                    self_name,
                    is_async: *is_async,
                    args: vec![Value::Int(0), Value::Int(1), Value::Int(2)],
                    span,
                    depth: 0,
                })
            })
            .unwrap();
        assert!(matches!(entry_probe, Value::Int(3)));
        assert!(
            caller
                .with_value("entered_calls", span, |value| {
                    Ok(matches!(value, Value::Int(1)))
                })
                .unwrap(),
            "real op closure did not increment the observed counter"
        );
    }
    for counter in ["tail_calls", "entered_calls"] {
        caller.assign_owned(counter, Value::Int(0), span).unwrap();
        assert!(
            caller
                .with_value(counter, span, |value| {
                    Ok(matches!(value, Value::Int(0)))
                })
                .unwrap(),
            "counter did not reset before the Task oracle"
        );
    }
    assert!(interpreter.termination.is_none());
    assert!(interpreter.pending_fail.is_none());
    assert!(interpreter.caller_owners.is_empty());
    assert_eq!(interpreter.call_depth, 0);
    assert!(current_cleanup_context().is_none());
    assert!(current_task_cancellation().is_none());
    assert!(crate::runtime::task::current_execution_termination().is_none());

    let runtime = TaskRuntime::new();
    interpreter.task_runtime = Some(runtime.clone());
    let wait = Duration::from_secs(3);
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let (observed_tx, observed_rx) = mpsc::sync_channel(1);
    let child = runtime
        .spawn(move || {
            ready_tx
                .send(())
                .map_err(|_| KuError::runtime("fixture readiness receiver disappeared", span))?;
            // This is a finite containment wait, not a sleep-based deadline
            // assertion. The child cannot finish before explicit release.
            let released = release_rx.recv_timeout(wait).is_ok();
            let context = current_task_cancellation();
            let _ = observed_tx.send((released, context));
            if released {
                Ok(Value::Result {
                    ok: true,
                    value: Box::new(Value::Null),
                })
            } else {
                Err(KuError::runtime("fixture child release timed out", span))
            }
        })
        .unwrap();
    let ready = ready_rx.recv_timeout(wait);
    let reached_barrier = ready.is_ok();
    let outcome = (|| -> KuResult<bool> {
        ready.map_err(|_| KuError::runtime("fixture child did not reach barrier", span))?;
        caller.define_owned("value".into(), Value::Task(child), false, span)?;
        // Evaluate the actual result initializer only. Executing Probe/main
        // would introduce a separate owner teardown and invalidate the control.
        interpreter
            .eval(expression, &mut caller, 0)
            .map(|value| matches!(value, Value::Null))
    })();
    let local_state_restored = interpreter.termination.is_none()
        && current_cleanup_context().is_none()
        && current_task_cancellation().is_none()
        && crate::runtime::task::current_execution_termination().is_none();
    let call_bookkeeping_restored =
        interpreter.caller_owners.is_empty() && interpreter.call_depth == 0;
    let source_slot_is_task = caller.with_value("value", span, |value| {
        Ok(match value {
            Value::Task(_) => Some(true),
            Value::Null => Some(false),
            _ => None,
        })
    });
    let tail_calls = caller.with_value("tail_calls", span, |value| {
        Ok(match value {
            Value::Int(count) => Some(*count),
            _ => None,
        })
    });
    let entered_calls = caller.with_value("entered_calls", span, |value| {
        Ok(match value {
            Value::Int(count) => Some(*count),
            _ => None,
        })
    });
    // Move out the pending payload once. Inspection clones only its three
    // strings, never a Task or its owner. Keep the original until after the
    // child's cancellation observation so an unexpected payload drop cannot
    // manufacture the observation used below.
    let pending = interpreter.take_pending_fail();
    let pending_was_some = pending.is_some();
    let pending_payload = pending.as_ref().and_then(|value| {
        let Value::Object(fields) = value else {
            return None;
        };
        let text = |name| match fields.get(name) {
            Some(Value::String(text)) => Some(text.clone()),
            _ => None,
        };
        Some((
            fields.len(),
            text("domain")?,
            text("code")?,
            text("message")?,
        ))
    });
    let pending_taken = interpreter.pending_fail.is_none();
    // No semantic assertions after spawn occur before explicit release/drain,
    // including unexpected Ok/Err and wrong pending payloads. A Rust panic
    // relies on the bounded subprocess/finite child wait instead, not this path.
    let released = release_tx.send(());
    let observed = observed_rx.recv_timeout(wait);
    drop(pending);
    let drained = runtime.cancel_all_and_wait(Duration::from_secs(2));
    let snapshot = runtime.snapshot();
    drop(caller);
    drop(interpreter);
    drop(runtime);

    assert!(reached_barrier, "fixture child never reached the barrier");
    assert!(released.is_ok(), "fixture release receiver disappeared");
    let (explicit_release, child_context) = observed.expect("fixture child did not report release");
    assert!(
        explicit_release,
        "fixture child reached its containment timeout"
    );
    drained.expect("fixture runtime did not drain after explicit release");
    let snapshot = snapshot.unwrap();
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(snapshot.queued_blocking_jobs, 0);
    assert_eq!(snapshot.running_blocking_jobs, 0);
    assert!(
        local_state_restored,
        "local cleanup latched parent cancellation"
    );
    assert!(call_bookkeeping_restored, "call observers or depth leaked");
    assert!(pending_taken);
    assert_eq!(source_slot_is_task.unwrap(), Some(borrowed_from_caller));
    assert_eq!(tail_calls.unwrap(), Some(0), "third argument was evaluated");
    assert_eq!(
        entered_calls.unwrap(),
        Some(0),
        "first-class target body ran"
    );
    if !borrowed_from_caller {
        // The deliberately blocked call-owned Task exhausts the existing local
        // cleanup deadline. That cleanup error wins over either the ordinary
        // division error or the saved Flow::Fail payload; it is not a parent
        // runtime termination and cannot restore the discarded pending fail.
        let error = outcome.expect_err("unfinished call-owned Task escaped argument cleanup");
        assert_eq!(error.domain.as_deref(), Some("task"));
        assert_eq!(error.code.as_deref(), Some("shutdown_timeout"));
        assert!(error.runtime_termination().is_none());
        let context = error
            .scope_cleanup()
            .expect("temporary owner lost its cleanup deadline");
        assert_eq!(context.reason, TerminationReason::Cancelled);
        assert_eq!(child_context, Some(context));
        assert!(
            !pending_was_some,
            "cleanup failure restored the discarded fail payload"
        );
    } else {
        assert!(
            child_context.is_none(),
            "argument cleanup cancelled its caller's borrowed Task"
        );
        match kind {
            InterruptedArgumentKind::OrdinaryError => {
                let error = outcome.expect_err("ordinary argument error was lost");
                assert_eq!(error.message, "division by zero");
                assert!(error.runtime_termination().is_none());
                assert!(error.scope_cleanup().is_none());
                assert!(!pending_was_some);
            }
            InterruptedArgumentKind::QuestionFail => {
                assert!(outcome.expect("question failure should remain recoverable"));
                assert!(pending_was_some, "question failure payload was dropped");
                assert_eq!(
                    pending_payload,
                    Some((
                        3,
                        "fixture.arguments".into(),
                        "late_failure".into(),
                        "late-argument".into(),
                    ))
                );
            }
        }
    }
}

#[test]
fn interpreter_named_argument_error_temporary_task_is_settled() {
    assert_interrupted_call_arguments(
        InterruptedArgumentKind::OrdinaryError,
        InterruptedArgumentStyle::Named,
        false,
    );
}

#[test]
fn interpreter_named_argument_error_borrowed_task_remains_with_caller() {
    assert_interrupted_call_arguments(
        InterruptedArgumentKind::OrdinaryError,
        InterruptedArgumentStyle::Named,
        true,
    );
}

#[test]
fn interpreter_first_class_argument_error_temporary_task_is_settled() {
    assert_interrupted_call_arguments(
        InterruptedArgumentKind::OrdinaryError,
        InterruptedArgumentStyle::FirstClass,
        false,
    );
}

#[test]
fn interpreter_first_class_argument_error_borrowed_task_remains_with_caller() {
    assert_interrupted_call_arguments(
        InterruptedArgumentKind::OrdinaryError,
        InterruptedArgumentStyle::FirstClass,
        true,
    );
}

#[test]
fn interpreter_named_argument_question_temporary_task_discards_pending_fail() {
    assert_interrupted_call_arguments(
        InterruptedArgumentKind::QuestionFail,
        InterruptedArgumentStyle::Named,
        false,
    );
}

#[test]
fn interpreter_named_argument_question_borrowed_task_preserves_pending_fail() {
    assert_interrupted_call_arguments(
        InterruptedArgumentKind::QuestionFail,
        InterruptedArgumentStyle::Named,
        true,
    );
}

#[test]
fn interpreter_first_class_argument_question_temporary_task_discards_pending_fail() {
    assert_interrupted_call_arguments(
        InterruptedArgumentKind::QuestionFail,
        InterruptedArgumentStyle::FirstClass,
        false,
    );
}

#[test]
fn interpreter_first_class_argument_question_borrowed_task_preserves_pending_fail() {
    assert_interrupted_call_arguments(
        InterruptedArgumentKind::QuestionFail,
        InterruptedArgumentStyle::FirstClass,
        true,
    );
}

fn service_fixture() -> Value {
    Value::Object(HashMap::from([
        ("kind".into(), Value::String("http.service".into())),
        ("routes".into(), Value::Array(Vec::new())),
    ]))
}

#[test]
fn interpreter_task_cleanup_rejects_listener_work_but_allows_synchronous_close() {
    let _execution = ExecutionTerminationGuard::enter();
    let span = Span::default();
    let service = service_fixture();
    let router = compile_http_routes(&service, span).unwrap();
    let (id, address) = bind_http_address("127.0.0.1:0").unwrap();
    let listener = http_listener_value(id, address, service.clone(), router);
    let mut env = Env::new();
    env.define_owned("app".into(), service, false, span)
        .unwrap();
    env.define_owned("listener".into(), listener, false, span)
        .unwrap();
    let mut interpreter = Interpreter::new();
    let context = CancellationContext::new(TerminationReason::Cancelled);
    let _cleanup = CleanupGuard::enter(context);
    for source in [
        "app.bind(\"127.0.0.1:0\")",
        "app.listen(\"127.0.0.1:0\")",
        "listener.run()",
    ] {
        let error = interpreter
            .exec_block(&finally_tests::body(source), &mut env, 0)
            .err()
            .expect("cleanup must reject listener submission");
        assert_eq!(
            error.runtime_termination(),
            Some(context),
            "{source}: {error}"
        );
    }
    interpreter
        .exec_block(&finally_tests::body("listener.close()?"), &mut env, 0)
        .unwrap();
    assert!(
        http_listener_registry::take(id, span).is_err(),
        "close did not release the socket"
    );
}

#[test]
fn interpreter_listener_polling_propagates_internal_timeout_instead_of_recoverable_error() {
    let _execution = ExecutionTerminationGuard::enter();
    let span = Span::default();
    let service = service_fixture();
    let router = compile_http_routes(&service, span).unwrap();
    let (id, address) = bind_http_address("127.0.0.1:0").unwrap();
    let listener = http_listener_value(id, address, service, router);
    let mut interpreter = Interpreter::new();
    interpreter.execution_deadline = Some(HttpHandlerDeadline::new(Instant::now()));
    let result = interpreter.run_http_listener(listener, span);
    let error = result_from_listener_operation(result, "run_failed").unwrap_err();
    assert_eq!(
        error.runtime_termination().unwrap().reason,
        TerminationReason::TimedOut
    );
    assert!(http_listener_registry::take(id, span).is_err());
}

#[test]
fn interpreter_http_request_timeout_does_not_cancel_listener_callers_tasks() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::new();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let outer = runtime
        .spawn(move || {
            ready_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            Ok(Value::Int(7))
        })
        .unwrap();
    ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let mut server_env = Env::new();
    server_env
        .define_owned(
            "outer".into(),
            Value::Task(outer.clone()),
            false,
            Span::default(),
        )
        .unwrap();
    let mut interpreter = Interpreter::new();
    interpreter.task_runtime = Some(runtime.clone());
    interpreter
        .caller_owners
        .push(server_env.observe_owned_bindings(Span::default()).unwrap());
    let handler = Value::Function {
        params: Vec::new(),
        param_modes: Vec::new(),
        body: finally_tests::body("try { while (true) {} } finally { finished = true }"),
        captures: Env::new(),
        self_name: None,
        is_async: false,
    };
    let result = interpreter.call_http_handler(
        handler,
        Value::Null,
        Span::default(),
        Instant::now() + Duration::from_millis(20),
    );
    let status = outer.status();
    release_tx.send(()).unwrap();
    assert_eq!(
        result.unwrap_err().runtime_termination().unwrap().reason,
        TerminationReason::TimedOut
    );
    assert_eq!(
        status, "running",
        "a request timeout reached the listener's owning scope"
    );
    assert_eq!(outer.await_result().unwrap(), Value::Int(7));
    assert_eq!(interpreter.caller_owners.len(), 1);
    assert!(crate::runtime::task::current_execution_termination().is_none());
    runtime.cancel_all_and_wait(Duration::from_secs(1)).unwrap();
}
