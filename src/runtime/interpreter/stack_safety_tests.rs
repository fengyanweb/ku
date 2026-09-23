use super::*;

const NAMED_RECURSION: &str = r#"
fn Descend(n: int): int {
    if (n <= 0) { return 0 }
    return Descend(n - 1)
}
fn main() {}
"#;

enum ExpectedOutcome {
    Returned(i64),
    CallDepthExceeded,
}

// Each case runs in its own bounded process: Rust stack overflow aborts rather
// than unwinds. A child crash is always a failed assertion, never the oracle.
fn run_worker_case(
    test_name: &'static str,
    source: &'static str,
    entry: &'static str,
    argument: i64,
    expected: ExpectedOutcome,
) {
    // module_path! includes the crate name; libtest's --exact names do not.
    let (_, test_name) = test_name
        .split_once("::")
        .expect("stack regression must have a module-qualified test name");
    const CHILD: &str = "KU_INTERPRETER_STACK_SAFETY_CHILD";
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
        .expect("worker stack regression cannot hang the test supervisor");
        assert!(
            output.status.success(),
            "worker stack regression child must exit normally; actual status: {:?}, code: {:?}\n{}\n{}",
            output.status,
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let completed = format!("worker stack regression completed: {test_name}");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .any(|line| line == completed),
            "child exited without completing this exact case (zero selected tests is not a pass)\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let program = Parser::new(Lexer::new(source).tokenize().unwrap())
        .parse_program()
        .unwrap();
    crate::checker::Checker::new().check(&program).unwrap();
    let mut interpreter = Interpreter::new();
    for item in program.items {
        let Item::Function(function) = item else {
            panic!("stack regression fixture contains only top-level functions");
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
            interpreter.call_function(entry, vec![Value::Int(argument)], Span::default(), 0);
        let _ = result_tx.send((result, interpreter.call_depth));
        Ok(Value::Null)
    });
    // Do not await/help from the parent: the ordinary worker must execute this
    // call, rather than a CLI thread with a separately enlarged stack.
    let ready = ready_rx.recv_timeout(Duration::from_secs(3));
    if let Ok((id, name)) = &ready {
        eprintln!("worker stack regression entered: {name} {id:?}; case: {test_name}");
    }
    let outcome = result_rx.recv_timeout(Duration::from_secs(3));
    let admission_error = task.as_ref().err().cloned();
    // Drain ordinary failures before assertions. If the child aborts, the
    // containing parent reports failure; no successful drain can be inferred.
    drop(task);
    let drained = runtime.cancel_all_and_wait(Duration::from_secs(2));
    let snapshot = runtime.snapshot();
    drained.expect("worker stack regression task must drain");
    let snapshot = snapshot.unwrap();
    assert!(admission_error.is_none(), "{admission_error:?}");
    let (worker_id, worker_name) = ready.expect("a real worker must report entry");
    assert_ne!(worker_id, parent_id);
    assert!(worker_name.starts_with("ku-task-"), "{worker_name}");
    let (result, depth) = outcome.expect("worker must finish within the channel bound");
    match expected {
        ExpectedOutcome::Returned(value) => {
            assert_eq!(
                result.expect("within-limit recursion must return"),
                Value::Int(value)
            );
        }
        ExpectedOutcome::CallDepthExceeded => {
            let error = result.expect_err("recursion past MAX_CALL_DEPTH must reject");
            assert_eq!(error.domain.as_deref(), Some("runtime"));
            assert_eq!(error.code.as_deref(), Some("call_depth_exceeded"));
        }
    }
    assert_eq!(depth, 0);
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(snapshot.queued_blocking_jobs, 0);
    assert_eq!(snapshot.running_blocking_jobs, 0);
    eprintln!("worker stack regression completed: {test_name}");
}

#[test]
fn worker_named_recursion_original_over_limit_returns_structured_error() {
    run_worker_case(
        concat!(
            module_path!(),
            "::worker_named_recursion_original_over_limit_returns_structured_error"
        ),
        NAMED_RECURSION,
        "Descend",
        1024,
        ExpectedOutcome::CallDepthExceeded,
    );
}

#[test]
fn worker_named_non_tail_recursion_resumes_return_expression() {
    let source = r#"
fn Sum(n: int): int {
    if (n <= 0) { return 0 }
    return n + Sum(n - 1)
}
fn main() {}
"#;
    run_worker_case(
        concat!(
            module_path!(),
            "::worker_named_non_tail_recursion_resumes_return_expression"
        ),
        source,
        "Sum",
        32,
        ExpectedOutcome::Returned(528),
    );
}

#[test]
fn worker_named_recursion_allows_exactly_512_active_calls() {
    assert_eq!(MAX_CALL_DEPTH, 512);
    // Direct entry uses depth zero. Descend(511) through Descend(0) has 512
    // active calls; there is no additional source-language main call here.
    run_worker_case(
        concat!(
            module_path!(),
            "::worker_named_recursion_allows_exactly_512_active_calls"
        ),
        NAMED_RECURSION,
        "Descend",
        511,
        ExpectedOutcome::Returned(0),
    );
}

#[test]
fn worker_named_recursion_rejects_first_call_past_512() {
    assert_eq!(MAX_CALL_DEPTH, 512);
    run_worker_case(
        concat!(
            module_path!(),
            "::worker_named_recursion_rejects_first_call_past_512"
        ),
        NAMED_RECURSION,
        "Descend",
        512,
        ExpectedOutcome::CallDepthExceeded,
    );
}

#[test]
fn worker_function_value_recursion_over_limit_returns_structured_error() {
    let source = r#"
fn Entry(n: int): int {
    fn LocalDescend(remaining: int): int {
        if (remaining <= 0) { return 0 }
        return LocalDescend(remaining - 1)
    }
    return LocalDescend(n)
}
fn main() {}
"#;
    // A local named function is represented by Value::Function with self_name.
    // LocalDescend is not a top-level entry in interpreter.functions, so its
    // recursive calls must traverse the function-value dispatch path.
    run_worker_case(
        concat!(
            module_path!(),
            "::worker_function_value_recursion_over_limit_returns_structured_error"
        ),
        source,
        "Entry",
        1024,
        ExpectedOutcome::CallDepthExceeded,
    );
}
