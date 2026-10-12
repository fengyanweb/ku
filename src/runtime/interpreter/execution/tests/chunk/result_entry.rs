use super::*;

const RESULT_ENTRY: &str = r#"
fn Sum(n: int): int {
    if (n <= 0) { return 0 }
    return n + Sum(n - 1)
}
fn Calc(n: int): int! {
    try { return ok(Sum(n)) } finally { retired = 1 }
    return ok(-1)
}
async fn AsyncCalc(n: int): int! {
    try { return ok(Sum(n)) } finally { retired = 1 }
    return ok(-1)
}
"#;

fn install(host: &mut Interpreter, source: &HashMap<String, FnDecl>) {
    for (name, function) in source {
        host.functions.insert(name.clone(), function.clone());
    }
}

#[test]
fn scalar_result_entry_keeps_real_ok_ticks_finally_and_512_boundary_across_chunks() {
    let source = functions(RESULT_ENTRY);
    for argument in [510, 511] {
        let args = [Value::Int(argument)];
        let mut legacy = Interpreter::new();
        install(&mut legacy, &source);
        let expected =
            legacy.call_function_direct(source["Calc"].clone(), args.to_vec(), Span::default(), 0);
        for name in ["Calc", "AsyncCalc"] {
            let function = &source[name];
            let code = if function.is_async {
                assert!(code::eligible_claimed_async_entry(
                    &source,
                    function,
                    Limits::default()
                ));
                CodeHandle::compile_claimed_async_entry(&source, function, Limits::default())
                    .unwrap()
            } else {
                assert!(code::eligible_named(&source, function, Limits::default()));
                CodeHandle::compile_named(&source, function, Limits::default()).unwrap()
            };
            for size in [1, 7, 256, 4096] {
                let mut machine = Machine::from_code(code.clone(), Limits::default(), 0);
                let mut hooks = TraceHooks::new();
                install(&mut hooks.host, &source);
                machine.start_once(name, &args, 0, false).unwrap();
                let (actual, more) =
                    drive_chunks(&mut machine, &mut hooks, NonZeroUsize::new(size).unwrap());
                assert_eq!(actual, expected, "{name}, {argument}, {size}");
                assert_eq!(
                    hooks.host.steps, legacy.steps,
                    "builtin ok cannot add a callee or argument epilogue"
                );
                assert_eq!(machine.metrics.peak_calls, 512);
                assert_drained(&machine.metrics, 0);
                if size == 1 {
                    assert!(more > 512);
                }
                let call_expr = match &function.body[0] {
                    Stmt::Try { body, .. } => match &body[0] {
                        Stmt::Return {
                            value: Some(value), ..
                        } => value,
                        _ => unreachable!(),
                    },
                    _ => unreachable!(),
                };
                let ExprKind::Call { callee, .. } = &call_expr.kind else {
                    unreachable!()
                };
                assert!(hooks.ticks.contains(&call_expr.span));
                assert!(
                    !hooks.ticks.contains(&callee.span),
                    "builtin ok does not evaluate its callee"
                );
            }
        }
    }
}

#[test]
fn result_roots_preserve_whole_tree_boundaries_and_budget_rejection() {
    for text in [
        "fn Entry(value: int!): int! { return value }",
        "fn Entry(): int! { value = ok(3)\n return value }",
        "fn Entry(n: int): int! { if (n > 0) { return Entry(n - 1) }\n return ok(0) }",
        "fn ok(value: int): int! { fail(\"shadow\") }\nfn Entry(): int! { return ok(3) }",
        "async fn Child(): int! { return ok(1) }\nasync fn Entry(): int! { child = Child()\n return ok((await child)?) }",
    ] {
        let source = functions(text);
        let function = &source["Entry"];
        let (eligible, compiled) = if function.is_async {
            (code::eligible_claimed_async_entry(&source, function, Limits::default()), CodeHandle::compile_claimed_async_entry(&source, function, Limits::default()))
        } else {
            (code::eligible_named(&source, function, Limits::default()), CodeHandle::compile_named(&source, function, Limits::default()))
        };
        assert!(!eligible, "{text}");
        assert!(matches!(compiled, Err(AdmissionError::Unsupported(_))), "{text}: {compiled:?}");
    }
    let source = functions(RESULT_ENTRY);
    assert!(
        !code::eligible_named(&source, &source["AsyncCalc"], Limits::default()),
        "ordinary calls must not execute async bodies synchronously"
    );
    let limits = Limits {
        syntax: FrameLimits {
            max_frames: 16 * 1024,
            max_storage_bytes: 128,
        },
        ..Limits::default()
    };
    assert!(matches!(
        CodeHandle::compile_named(&source, &source["Calc"], limits),
        Err(AdmissionError::Rejected(_))
    ));
    let code = CodeHandle::compile_named(&source, &source["Calc"], Limits::default()).unwrap();
    let mut machine = Machine::from_code(
        code,
        Limits {
            work: FrameLimits {
                max_frames: 1,
                max_storage_bytes: 2 * 1024 * 1024,
            },
            ..Limits::default()
        },
        0,
    );
    let mut hooks = TraceHooks::new();
    install(&mut hooks.host, &source);
    machine
        .start_once("Calc", &[Value::Int(510)], 0, false)
        .unwrap();
    let (result, _) = drive_chunks(&mut machine, &mut hooks, NonZeroUsize::new(1).unwrap());
    assert!(result
        .unwrap_err()
        .message
        .contains("internal execution storage rejected"));
    assert!(
        hooks.events.is_empty(),
        "entry budget rejection cannot run or restart a body"
    );
    assert_drained(&machine.metrics, 0);
}

#[test]
fn actual_named_async_factory_selects_machine_and_returns_unique_result() {
    use crate::runtime::task::LocalTaskRunner;
    use std::sync::mpsc;
    let source = functions(RESULT_ENTRY);
    let runtime = TaskRuntime::new();
    let mut parent = Interpreter::new();
    install(&mut parent, &source);
    parent.task_runtime = Some(runtime.clone());
    let child_runtime = runtime.clone();
    let function = source["AsyncCalc"].clone();
    let (tx, rx) = mpsc::sync_channel(1);
    let task = runtime
        .spawn_runner_deferred(|| {
            let template = parent.template();
            move || {
                let interpreter = Interpreter::from_template(template, child_runtime);
                let mut runner = ClaimedNamedTask::new(
                    interpreter,
                    function,
                    vec![Value::Int(510)],
                    Span::default(),
                );
                assert!(
                    runner.machine.is_some(),
                    "real claimant selects the admitted production machine"
                );
                let progress = runner.dispatch(NonZeroUsize::new(1).unwrap());
                tx.send((
                    matches!(progress, std::ops::ControlFlow::Continue(())),
                    crate::runtime::task::current_task_id(),
                    std::mem::size_of::<ClaimedNamedTask>(),
                ))
                .unwrap();
                runner
            }
        })
        .unwrap();
    let result = task.await_result();
    let witness = rx.recv_timeout(std::time::Duration::from_secs(2));
    let second = task.await_result();
    let original_id = task.id();
    drop(task);
    let shutdown = runtime.cancel_all_and_wait(std::time::Duration::from_secs(2));
    let snapshot = runtime.snapshot().unwrap();
    shutdown.unwrap();
    let (more, task_id, bytes) = witness.unwrap();
    assert!(more);
    assert_eq!(task_id, original_id);
    assert_eq!(
        result.unwrap(),
        Value::Result {
            ok: true,
            value: Box::new(Value::Int(130305))
        }
    );
    assert!(matches!(second.unwrap(), Value::Result { ok: false, .. }));
    eprintln!("named claimant inline runner={bytes} machine={} Work={} CallFrame={} (excludes backing/source/payload/RSS)", std::mem::size_of::<Machine>(), std::mem::size_of::<Work>(), std::mem::size_of::<CallFrame>());
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.registered_tasks, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(snapshot.wait_edges, 0);
}
