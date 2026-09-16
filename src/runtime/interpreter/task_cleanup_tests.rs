use super::*;

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
