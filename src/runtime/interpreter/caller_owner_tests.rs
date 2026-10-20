use super::*;

#[test]
fn interpreter_callee_finally_observes_caller_child_cancellation_first() {
    assert_callee_finally_observes_caller_child_cancellation_first(None);
}

#[test]
fn interpreter_view_callee_finally_observes_borrowed_caller_task_owners_first() {
    let source = r#"
        fn Inspect<T>(&value: T) { println(value) }
        async fn Child(): str! { return ok("payload") }
        async fn main(): null! {
            handle = Child()
            Inspect(handle)
            payload = { child: Child() }
            Inspect(payload)
            return ok(null)
        }
    "#;
    let program = Parser::new(Lexer::new(source).tokenize().unwrap())
        .parse_program()
        .unwrap();
    crate::checker::Checker::new().check(&program).unwrap();
    for wrapped in [false, true] {
        assert_callee_finally_observes_caller_child_cancellation_first(Some(wrapped));
    }
}

fn assert_callee_finally_observes_caller_child_cancellation_first(borrowed: Option<bool>) {
    let runtime = TaskRuntime::new();
    let thread_runtime = runtime.clone();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (start_tx, start_rx) = mpsc::sync_channel(1);
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let (child_context_tx, child_context_rx) = mpsc::sync_channel(1);
    let parent = thread::spawn(move || {
        let _execution = ExecutionTerminationGuard::enter();
        let mut env = finally_tests::marker_env();
        let (child_started_tx, child_started_rx) = mpsc::sync_channel(1);
        let child = thread_runtime
            .spawn(move || {
                child_started_tx.send(()).unwrap();
                let end = Instant::now() + Duration::from_secs(3);
                loop {
                    if let Some(context) = crate::runtime::task::current_task_cancellation() {
                        child_context_tx.send(context).unwrap();
                        return Ok(Value::Null);
                    }
                    if Instant::now() >= end {
                        return Err(KuError::runtime(
                            "caller child cancellation was not observed",
                            Span::default(),
                        ));
                    }
                    thread::yield_now();
                }
            })
            .unwrap();
        child_started_rx
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        let child_value = if borrowed == Some(true) {
            Value::Object(HashMap::from([("task".into(), Value::Task(child.clone()))]))
        } else {
            Value::Task(child.clone())
        };
        env.define("child".into(), child_value, false, Span::default())
            .unwrap();
        let slow = Value::Function {
            params: if borrowed.is_some() { vec!["value".into()] } else { Vec::new() },
            param_modes: if borrowed.is_some() { vec![ParamMode::View] } else { Vec::new() },
            body: finally_tests::body("try { marker = 1\nwhile (true) {} } finally { marker = 2\nwhile (marker == 2) {} }"),
            captures: env.capture(&HashSet::from(["marker".into()])),
            self_name: None, is_async: false,
        };
        env.define("Slow".into(), slow, false, Span::default())
            .unwrap();
        ready_tx.send((env.clone(), child)).unwrap();
        start_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let mut interpreter = Interpreter::new();
        interpreter.task_runtime = Some(thread_runtime);
        interpreter.execution_deadline = Some(HttpHandlerDeadline::new(
            Instant::now() + Duration::from_millis(30),
        ));
        let call = if borrowed.is_some() {
            "Slow(child)"
        } else {
            "Slow()"
        };
        let result = interpreter.exec_block(&finally_tests::body(call), &mut env, 0);
        // Mirror the function boundary's final cleanup of its base parameter /
        // owner scope, not merely the nested exec_block scope containing Slow().
        let result = interpreter.finish_owned_scope(&mut env, result, Span::default(), false);
        let empty = interpreter.caller_owners.is_empty();
        done_tx.send(()).unwrap();
        (result.err().expect("callee loop must time out"), empty)
    });
    let (mut env, child) = ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    start_tx.send(()).unwrap();
    let end = Instant::now() + Duration::from_secs(3);
    while finally_tests::marker(&env) != 2 {
        assert!(Instant::now() < end, "callee did not reach finally");
        thread::yield_now();
    }
    let parent_still_in_finally = matches!(done_rx.try_recv(), Err(mpsc::TryRecvError::Empty));
    let child_status = child.status();
    // Always release the user cleanup barrier before asserting, including when
    // testing the old missing-caller-broadcast path.
    env.assign("marker", Value::Int(3), Span::default())
        .unwrap();
    let (error, empty) = parent.join().unwrap();
    assert!(
        parent_still_in_finally,
        "the observation must precede caller unwinding"
    );
    assert!(
        matches!(child_status, "cancelling" | "cancelled" | "timed_out"),
        "caller child was still {child_status} inside callee finally"
    );
    assert_eq!(
        error.runtime_termination().unwrap().reason,
        TerminationReason::TimedOut
    );
    assert_eq!(
        Some(
            child_context_rx
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
        ),
        error.runtime_termination(),
        "child and parent must share both the timeout reason and absolute cleanup deadline"
    );
    assert_released_observer(&child);
    assert!(empty, "the call observer must be removed after an error");
    runtime.cancel_all_and_wait(Duration::from_secs(1)).unwrap();
}

fn assert_released_observer(task: &crate::runtime::task::TaskHandle) {
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

#[test]
fn interpreter_scope_releases_completed_payload_while_readonly_capture_keeps_task_identity() {
    let runtime = TaskRuntime::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let listener_id = http_listener_registry::insert(listener).unwrap();
    let lease = crate::runtime::value::HttpListenerLease::new(listener_id);
    let payload_observer = Arc::downgrade(&lease);
    let child = runtime
        .spawn(move || {
            Ok(stdlib::errors::ok(Value::Object(HashMap::from([
                ("lease".into(), Value::HttpListenerLease(lease)),
                ("payload".into(), Value::String("owned".repeat(16_384))),
            ]))))
        })
        .unwrap();
    let child_id = child.id();
    let deadline = Instant::now() + Duration::from_secs(3);
    while child.status() != "completed" {
        assert!(Instant::now() < deadline, "fixture child did not complete");
        thread::yield_now();
    }
    assert!(payload_observer.upgrade().is_some());
    let span = Span::default();
    let mut owner = Env::new();
    owner.push_scope();
    owner
        .define_owned("handle".into(), Value::Task(child), false, span)
        .unwrap();
    let capture = owner.capture(&HashSet::from(["handle".into()]));
    assert!(capture.all_owned_tasks(span).unwrap().is_empty());
    let mut interpreter = Interpreter::new();
    interpreter.task_runtime = Some(runtime);
    interpreter
        .finish_owned_scope(&mut owner, Ok(Flow::Continue), span, true)
        .unwrap();
    assert!(!owner.contains("handle"));
    assert!(
        payload_observer.upgrade().is_none(),
        "the surviving captured cell must not retain the completed owned payload"
    );
    assert!(http_listener_registry::take(listener_id, span).is_err());
    let Value::Task(observed) = capture.get("handle", span).unwrap() else {
        panic!("readonly captures must retain the Task identity control block")
    };
    assert_eq!(observed.id(), child_id);
    assert_eq!(observed.status(), "completed");
    assert_released_observer(&observed);
}

#[test]
fn interpreter_call_observers_are_removed_after_normal_and_argument_error_returns() {
    let mut interpreter = Interpreter::new();
    let mut env = Env::new();
    env.define("value".into(), Value::Int(1), false, Span::default())
        .unwrap();
    for source in ["len([1, 2])", "len(missing)"] {
        let result = interpreter.exec_block(&finally_tests::body(source), &mut env, 0);
        assert_eq!(result.is_ok(), source == "len([1, 2])");
        assert!(interpreter.caller_owners.is_empty(), "{source}");
    }
}
