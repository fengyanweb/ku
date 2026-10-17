use super::super::code::{AdmissionError, CodeHandle};
use super::*;

fn require_compiled(result: Result<CodeHandle, AdmissionError>) -> CodeHandle {
    match result {
        Ok(code) => code,
        Err(AdmissionError::Unsupported(span)) => {
            panic!("the closed scalar fixture must be supported at {span:?}")
        }
        Err(AdmissionError::Rejected(error)) => {
            panic!("the closed scalar fixture must be admitted: {error:?}")
        }
    }
}

fn compile_entry(source: &HashMap<String, FnDecl>, entry: &str, limits: Limits) -> CodeHandle {
    require_compiled(CodeHandle::compile_named(
        source,
        source.get(entry).expect("checked entry function"),
        limits,
    ))
}

#[test]
fn code_handle_survives_source_drop_for_self_and_mutual_recursion_at_512_calls() {
    for text in [
        RECURSION,
        r#"
fn Descend(n: int): int {
    if (n <= 0) { return 0 }
    return Other(n - 1)
}
fn Other(n: int): int {
    if (n <= 0) { return 0 }
    return Descend(n - 1)
}
"#,
    ] {
        let code = {
            let source = functions(text);
            compile_entry(&source, "Descend", Limits::default())
        };
        let code_bytes = code.retained_bytes();
        assert!(code_bytes <= Limits::default().syntax.max_storage_bytes);
        // Neither the original table nor its recursive AST is alive here.
        let mut machine = Machine::from_code(code, Limits::default(), 0);
        assert_eq!(
            machine.run("Descend", &[Value::Int(511)], 0).unwrap(),
            Value::Int(0)
        );
        assert_eq!(machine.metrics.peak_calls, 512);
        assert_eq!(machine.metrics.code_retained_backing_bytes, code_bytes);
        eprintln!(
            "persistent code at 512 calls: retained={} construction_upper={} call_frame={} work={} execution_retained={} execution_peak_upper={}",
            code_bytes,
            machine.metrics.code_construction_peak_backing_bytes_upper_bound,
            std::mem::size_of::<CallFrame>(),
            std::mem::size_of::<Work>(),
            machine.metrics.retained_backing_bytes,
            machine.metrics.peak_backing_bytes_upper_bound
        );
        assert_drained(&machine.metrics, 0);
        assert_depth_error(machine.run("Descend", &[Value::Int(512)], 0));
        assert_drained(&machine.metrics, 0);
    }
}

#[test]
fn code_handles_keep_arena_identity_and_source_versions_independent() {
    let mut source = functions("fn Entry(): int { return 7 }");
    let first = compile_entry(&source, "Entry", Limits::default());
    let other_source = functions("fn Entry(): int { return 19 }");
    let other = compile_entry(&other_source, "Entry", Limits::default());
    assert_eq!(first.find_function("Entry").unwrap().0, 0);
    assert_eq!(other.find_function("Entry"), first.find_function("Entry"));
    drop(other_source);
    source.insert(
        "Entry".into(),
        functions("fn Entry(): int { return 31 }")
            .remove("Entry")
            .unwrap(),
    );
    let updated = compile_entry(&source, "Entry", Limits::default());
    drop(source);

    // Identical entry names and first-node positions in separate arenas must
    // resolve through each handle's owner, including after source mutation.
    for (code, expected) in [(first.clone(), 7), (other, 19), (updated, 31), (first, 7)] {
        let mut machine = Machine::from_code(code, Limits::default(), 0);
        assert_eq!(machine.run("Entry", &[], 0).unwrap(), Value::Int(expected));
        assert_drained(&machine.metrics, 0);
    }
}

fn dismantle_unary_input(source: &mut HashMap<String, FnDecl>) {
    let Stmt::Return { value, .. } = &mut source.get_mut("Entry").unwrap().body[0] else {
        panic!("raw deep fixture must keep its original return");
    };
    let mut current = value.take().expect("raw unary expression");
    loop {
        match current.kind {
            ExprKind::Unary { expr, .. } => current = *expr,
            ExprKind::Literal(Literal::Int(1)) => break,
            _ => panic!("raw unary fixture has one scalar leaf"),
        }
    }
}

#[test]
fn code_handle_deep_unary_construction_execution_and_drop_finish_on_a_real_worker() {
    let (_, test_name) = concat!(
        module_path!(),
        "::code_handle_deep_unary_construction_execution_and_drop_finish_on_a_real_worker"
    )
    .split_once("::")
    .expect("deep worker regression needs an exact libtest name");
    if !bounded_worker_child(test_name) {
        return;
    }
    let runtime = TaskRuntime::new();
    let parent = thread::current().id();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let task = runtime.spawn(move || {
        let worker = thread::current();
        let _ = ready_tx.send((worker.id(), worker.name().unwrap_or("").to_owned()));
        let result = (|| -> KuResult<Value> {
            // Stay within the unchanged 1 MiB shared syntax budget while
            // exercising a raw AST much deeper than the parser permits.
            const DEPTH: usize = 1024;
            let mut source = functions("fn Entry(): int { return 1 }");
            let Stmt::Return { value, .. } = &mut source.get_mut("Entry").unwrap().body[0] else {
                panic!("checked fixture return");
            };
            let mut expression = value.take().unwrap();
            for _ in 0..DEPTH {
                let span = expression.span;
                expression = Expr::new(
                    ExprKind::Unary {
                        op: UnaryOp::Negate,
                        expr: Box::new(expression),
                    },
                    span,
                );
            }
            *value = Some(expression);
            // Exercise the internal constructor without changing the parser's
            // existing depth limit. Avoid recursive Clone or Drop of this raw
            // input even when compilation reports an ordinary failure.
            let compiled =
                CodeHandle::compile_named(&source, source.get("Entry").unwrap(), Limits::default());
            dismantle_unary_input(&mut source);
            drop(source);
            let code = require_compiled(compiled);
            let mut machine = Machine::from_code(code, Limits::default(), 0);
            let value = machine.run("Entry", &[], 0)?;
            assert_drained(&machine.metrics, 0);
            // Publish completion only after the last flat-code owner drops.
            drop(machine);
            Ok(value)
        })();
        let _ = result_tx.send(result);
        Ok(Value::Null)
    });
    // The supervisor never awaits/helps this job on its own stack.
    let ready = ready_rx.recv_timeout(Duration::from_secs(3));
    let outcome = result_rx.recv_timeout(Duration::from_secs(3));
    let admission = task.as_ref().err().cloned();
    drop(task);
    let drained = runtime.cancel_all_and_wait(Duration::from_secs(2));
    let snapshot = runtime.snapshot();
    drained.unwrap();
    let snapshot = snapshot.unwrap();
    assert!(admission.is_none(), "{admission:?}");
    let (id, name) = ready.expect("a real worker must enter the deep constructor");
    assert_ne!(id, parent);
    assert!(name.starts_with("ku-task-"), "{name}");
    assert_eq!(
        outcome.expect("worker must finish flat-code drop").unwrap(),
        Value::Int(1)
    );
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(snapshot.queued_blocking_jobs, 0);
    assert_eq!(snapshot.running_blocking_jobs, 0);
    eprintln!("worker regression completed: {test_name}");
}

#[test]
fn code_handle_long_symbol_aggregate_budget_rejects_and_allows_a_healthy_retry() {
    let symbol = "x".repeat(16 * 1024);
    let source = functions(&format!(
        "fn Entry(): int {{ {symbol} = 1\nreturn {symbol} }}"
    ));
    let mut limits = Limits::default();
    limits.syntax.max_storage_bytes = 8 * 1024;
    let rejected = CodeHandle::compile_named(&source, source.get("Entry").unwrap(), limits);
    match rejected {
        Err(AdmissionError::Rejected(_)) => {}
        Err(AdmissionError::Unsupported(span)) => {
            panic!(
                "a long legal identifier must report budget rejection, not unsupported: {span:?}"
            )
        }
        Ok(_) => panic!("owned long-symbol storage must be charged before body execution"),
    }
    let tiny_source = functions("fn Entry(): int { return 1 }");
    let mut count = Limits::default();
    // This tiny source has no individual buffer wider than four records;
    // its combined code and builder metadata still exceeds four frames.
    count.syntax.max_frames = 4;
    match CodeHandle::compile_named(&tiny_source, tiny_source.get("Entry").unwrap(), count) {
        Err(AdmissionError::Rejected(_)) => {}
        Err(AdmissionError::Unsupported(span)) => {
            panic!("legal tiny syntax must fail aggregate admission, not eligibility: {span:?}")
        }
        Ok(_) => panic!("separate arena buffers must share one logical-frame allowance"),
    }
    let source = functions("fn Entry(): int { value = 1\nreturn value }");
    let code = compile_entry(&source, "Entry", limits);
    drop(source);
    let mut machine = Machine::from_code(code, limits, 0);
    assert_eq!(machine.run("Entry", &[], 0).unwrap(), Value::Int(1));
    assert_drained(&machine.metrics, 0);
}

#[test]
fn production_code_admission_keeps_unsupported_fallback_separate_from_supported_budget_failure() {
    for text in [
        "fn Entry(): int { try { marker = 1 } catch (err) { marker = 2 } finally { marker = 3 }\nreturn 7 }",
        "fn Entry(): int { owned = \"owned\"\nreturn 7 }",
    ] {
        let mut interpreter = production_interpreter(text);
        let entry = interpreter.functions.get_mut("Entry").unwrap();
        let mut tail = std::mem::take(&mut entry.body);
        let mut body = Vec::with_capacity(15000 + tail.len());
        for _ in 0..15000 {
            body.push(Stmt::Expr {
                expr: Expr::new(ExprKind::Literal(Literal::Int(0)), Span::default()),
                span: Span::default(),
            });
        }
        body.append(&mut tail);
        entry.body = body;
        // Preserve one actual source-table snapshot without cloning the large
        // AST. The original small tail was parser/checker validated above.
        let snapshot = interpreter.functions.clone();
        let entry = snapshot.get("Entry").unwrap();
        match CodeHandle::compile_named(&snapshot, entry, Limits::default()) {
            Err(AdmissionError::Unsupported(_) | AdmissionError::Rejected(_)) => {}
            Ok(_) => panic!("an unsupported complete tree cannot publish flat code"),
        }
        // Streaming production eligibility must reach the legal unsupported
        // tail before deciding whether arena construction budgets apply.
        assert!(try_execute(&mut interpreter, entry, &[], 0).is_none());
        assert_eq!(interpreter.steps, 0);
        assert_eq!(interpreter.call_depth, 0);
    }

    let mut interpreter = production_interpreter("fn Entry(): int { value = 1\nreturn value }");
    let symbol = "x".repeat(Limits::default().syntax.max_storage_bytes + 1);
    let entry = interpreter.functions.get_mut("Entry").unwrap();
    let Stmt::Assign { name, .. } = &mut entry.body[0] else {
        panic!("checked implicit binding uses the actual Assign AST");
    };
    *name = symbol.clone();
    let Stmt::Return {
        value: Some(value), ..
    } = &mut entry.body[1]
    else {
        panic!("checked fixture returns the same scalar binding");
    };
    value.kind = ExprKind::Variable(symbol);
    let snapshot = interpreter.functions.clone();
    let error = match try_execute(&mut interpreter, snapshot.get("Entry").unwrap(), &[], 0) {
        Some(Err(error)) => error,
        None => panic!("supported syntax cannot bypass code budgets through legacy fallback"),
        Some(Ok(_)) => panic!("a symbol larger than the original syntax byte budget must reject"),
    };
    assert!(
        error.message.contains("storage") || error.message.contains("budget"),
        "{error:?}"
    );
    assert_eq!(interpreter.steps, 0);
    assert_eq!(interpreter.call_depth, 0);
    drop(snapshot);
    interpreter.functions.insert(
        "Entry".into(),
        functions("fn Entry(): int { return 7 }")
            .remove("Entry")
            .unwrap(),
    );
    assert_eq!(
        interpreter
            .call_function("Entry", vec![], Span::default(), 0)
            .unwrap(),
        Value::Int(7)
    );
    assert_eq!(interpreter.call_depth, 0);
}

#[test]
fn code_handle_local_source_drop_keeps_shared_capture_cells_without_owner_cloning() {
    let runtime = TaskRuntime::new();
    let task = runtime
        .spawn(|| {
            let mut interpreter = production_interpreter(
                r#"
fn Make() {
    captured = 20
    fn local(n: int): int {
        captured = captured + 1
        if (n <= 0) { return captured }
        return n + local(n - 1)
    }
    return local.clone()
}
"#,
            );
            let value = interpreter.call_function("Make", vec![], Span::default(), 0)?;
            let call = function_value_call(&value, 3, 0);
            assert_ne!(crate::runtime::task::current_task_id(), 0);
            assert!(call.captures.is_stable_scalar_binding("captured"));
            let captures = call.captures.clone();
            assert!(captures.observe_owned_bindings(Span::default())?.is_empty());
            let code = require_compiled(CodeHandle::compile_local(
                &interpreter.functions,
                &call,
                Limits::default(),
            ));
            drop(call);
            drop(value);
            drop(interpreter);

            let mut machine = Machine::from_code(code, Limits::default(), 0);
            machine.captures = Some(captures.clone());
            assert!(machine
                .captures
                .as_ref()
                .unwrap()
                .observe_owned_bindings(Span::default())?
                .is_empty());
            let result = machine.run("local", &[Value::Int(3)], 0)?;
            assert_drained(&machine.metrics, 0);
            let updated = captures.get("captured", Span::default())?;
            drop(machine);
            Ok(Value::Array(vec![result, updated]))
        })
        .unwrap();
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
}
