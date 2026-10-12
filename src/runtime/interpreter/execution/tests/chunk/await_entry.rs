use super::*;
use crate::runtime::task::{AwaitTicket, LocalTaskRunner, TaskRuntimeSnapshot};
use std::{ops::ControlFlow, sync::mpsc, thread::ThreadId, time::Duration};

const SOURCE: &str = r#"
async fn Child(n: int): int! {
    total = 0
    for i in n { total = total + i }
    return ok(total)
}
async fn Parent(n: int): int! {
    task = Child(n)
    moved = task
    try { return ok((await moved)? + 1) } finally { retired = 1 }
    return ok(-1)
}
async fn Broken(n: int): int! { return ok(10 / n) }
async fn Failure(n: int): int! {
    task = Broken(n)
    try { return ok((await task)?) } finally { retired = 2 }
    return ok(-1)
}
"#;

enum Witness {
    Waiting {
        thread: ThreadId,
        task: i64,
        pending: usize,
        edges: usize,
        waiting: usize,
    },
    Finished {
        thread: ThreadId,
        task: i64,
        steps: usize,
        calls: usize,
        retired: usize,
    },
}

struct SourceRunner {
    inner: ClaimedNamedTask,
    runtime: TaskRuntime,
    witness: mpsc::Sender<Witness>,
    reported_wait: bool,
}

impl LocalTaskRunner for SourceRunner {
    fn dispatch(&mut self, _budget: NonZeroUsize) -> ControlFlow<KuResult<Value>> {
        let result = self.inner.dispatch(NonZeroUsize::new(1).unwrap());
        let machine = self
            .inner
            .machine
            .as_ref()
            .expect("production source machine");
        if machine.state == MachineState::Waiting && !self.reported_wait {
            self.reported_wait = true;
            let snapshot = self.runtime.snapshot().unwrap();
            self.witness
                .send(Witness::Waiting {
                    thread: std::thread::current().id(),
                    task: crate::runtime::task::current_task_id(),
                    pending: snapshot.queued_tasks,
                    edges: snapshot.wait_edges,
                    waiting: snapshot.waiting_tasks,
                })
                .unwrap();
        }
        if matches!(result, ControlFlow::Break(_)) {
            eprintln!(
                "source await inline runner={} machine={} ticket={} Work={} CallFrame={} ArgumentFrame={} backing={} peak_backing={} code={} code_construction={} (excludes Env/payload/AST/RSS)",
                std::mem::size_of::<ClaimedNamedTask>(),
                std::mem::size_of::<Machine>(),
                std::mem::size_of::<AwaitTicket>(),
                std::mem::size_of::<Work>(),
                std::mem::size_of::<CallFrame>(),
                std::mem::size_of::<ArgumentFrame>(),
                machine.metrics.retained_backing_bytes,
                machine.metrics.peak_backing_bytes_upper_bound,
                machine.metrics.code_retained_backing_bytes,
                machine.metrics.code_construction_peak_backing_bytes_upper_bound,
            );
            self.witness
                .send(Witness::Finished {
                    thread: std::thread::current().id(),
                    task: crate::runtime::task::current_task_id(),
                    steps: self.inner.interpreter.steps,
                    calls: machine.metrics.entered_calls,
                    retired: machine.metrics.retired_calls,
                })
                .unwrap();
            assert_drained(&machine.metrics, 0);
        }
        result
    }

    fn waiting(&mut self) -> Option<&mut AwaitTicket> {
        self.inner.waiting()
    }
}

fn run_source(entry: &str, n: i64) -> (KuResult<Value>, Vec<Witness>, TaskRuntimeSnapshot) {
    let source = functions(SOURCE);
    let runtime = TaskRuntime::with_limits(1, 1024, 0, 0, 1024);
    let mut parent = Interpreter::new();
    result_entry::install(&mut parent, &source);
    parent.task_runtime = Some(runtime.clone());
    let child_runtime = runtime.clone();
    let factory_runtime = runtime.clone();
    let function = source[entry].clone();
    let (tx, rx) = mpsc::channel();
    let task = runtime
        .spawn_runner_deferred(|| {
            let template = parent.template();
            move || {
                let mut host = Interpreter::from_template(template, child_runtime);
                host.async_execution = true;
                let inner =
                    ClaimedNamedTask::new(host, function, vec![Value::Int(n)], Span::default());
                assert!(
                    inner.machine.is_some(),
                    "actual claimant must select shared code"
                );
                SourceRunner {
                    inner,
                    runtime: factory_runtime,
                    witness: tx,
                    reported_wait: false,
                }
            }
        })
        .unwrap();
    // The host must not act as an extra helper executor. Wait for the actual
    // claimant's finite witnesses before observing its published result.
    let first = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let second = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let result = task.await_result();
    let original_id = task.id();
    drop(task);
    runtime.cancel_all_and_wait(Duration::from_secs(2)).unwrap();
    let snapshot = runtime.snapshot().unwrap();
    let events = vec![first, second];
    for event in &events {
        match event {
            Witness::Waiting { task, .. } | Witness::Finished { task, .. } => {
                assert_eq!(*task, original_id);
            }
        }
    }
    (result, events, snapshot)
}

#[test]
fn actual_source_await_suspends_with_unique_moved_task_and_resumes_unticked() {
    let (result, events, snapshot) = run_source("Parent", 1000);
    assert_eq!(
        result.unwrap(),
        Value::Result {
            ok: true,
            value: Box::new(Value::Int(499501))
        }
    );
    let waiting = events
        .iter()
        .find_map(|event| match event {
            Witness::Waiting {
                thread,
                pending,
                edges,
                waiting,
                ..
            } => {
                assert_eq!((*pending, *edges, *waiting), (1, 1, 1));
                Some(*thread)
            }
            _ => None,
        })
        .expect("the actual parent must return Waiting before child help");
    let (thread, steps) = events
        .iter()
        .find_map(|event| match event {
            Witness::Finished {
                thread,
                steps,
                calls,
                retired,
                ..
            } => {
                assert_eq!((*calls, *retired), (1, 1));
                Some((*thread, *steps))
            }
            _ => None,
        })
        .expect("the retained parent must finish on the same claimant");
    assert_eq!(thread, waiting);
    let source = functions(SOURCE);
    let legacy_runtime = TaskRuntime::with_limits(1, 1024, 0, 0, 1024);
    let mut legacy = Interpreter::new();
    result_entry::install(&mut legacy, &source);
    legacy.task_runtime = Some(legacy_runtime.clone());
    legacy.async_execution = true;
    let expected = legacy.call_function_direct(
        source["Parent"].clone(),
        vec![Value::Int(1000)],
        Span::default(),
        0,
    );
    legacy_runtime
        .cancel_all_and_wait(Duration::from_secs(2))
        .unwrap();
    assert_eq!(
        expected.unwrap(),
        Value::Result {
            ok: true,
            value: Box::new(Value::Int(499501))
        }
    );
    assert_eq!(
        steps, legacy.steps,
        "await resume must not add expression ticks"
    );
    assert_eq!(
        (
            snapshot.active_tasks,
            snapshot.queued_tasks,
            snapshot.registered_tasks,
            snapshot.wait_edges
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(snapshot.finished_tasks, 2);
}

#[test]
fn actual_source_helper_failure_preserves_error_and_drains_both_tasks() {
    let (result, events, snapshot) = run_source("Failure", 0);
    assert_eq!(result.unwrap_err().message, "division by zero");
    assert!(events
        .iter()
        .any(|event| matches!(event, Witness::Waiting { .. })));
    assert_eq!(
        (
            snapshot.active_tasks,
            snapshot.queued_tasks,
            snapshot.registered_tasks,
            snapshot.wait_edges
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(snapshot.finished_tasks, 2);
}

#[test]
fn result_local_and_borrowed_parameter_stay_whole_tree_legacy() {
    for text in [
        "async fn Child(): int! { return ok(1) }\nasync fn Entry(): int! { result = await Child()\n return result }",
        "fn Read(&text: str, value: int): int { return value }\nasync fn Child(): int! { return ok(1) }\nasync fn Entry(): int! { text = \"hello\"\n return ok(Read(text, (await Child())?)) }",
    ] {
        let source = functions(text);
        assert!(!code::eligible_claimed_async_entry(&source, &source["Entry"], Limits::default()));
        assert!(matches!(CodeHandle::compile_claimed_async_entry(&source, &source["Entry"], Limits::default()), Err(AdmissionError::Unsupported(_))));
    }
}

#[test]
fn caller_task_is_cancelled_before_callee_protected_finally_runs() {
    let source = functions(
        r#"
async fn Child(): int! { return ok(1) }
fn Slow(): int {
    try { return 1 } finally { marker = 7 }
    return 0
}
fn Caller(): int! { task = Child()
    return ok(Slow())
}
"#,
    );
    let (trigger, finally_stmt) = match &source["Slow"].body[0] {
        Stmt::Try {
            body, finally_body, ..
        } => (stmt_span(&body[0]), stmt_span(&finally_body[0])),
        _ => unreachable!(),
    };
    let code = CodeHandle::compile_named(&source, &source["Caller"], Limits::default()).unwrap();
    let runtime = TaskRuntime::with_limits(1, 1024, 0, 0, 1024);
    let child_runtime = runtime.clone();
    let (tx, rx) = mpsc::sync_channel(1);
    let task = runtime
        .spawn(move || {
            // Occupy the actual sole worker while Caller queues Child; cancellation
            // must happen through the callee's genuine caller-binding observer.
            let mut hooks = TraceHooks::new();
            result_entry::install(&mut hooks.host, &source);
            hooks.host.task_runtime = Some(child_runtime);
            hooks.host.async_execution = true;
            hooks.timeout_at = Some(trigger);
            hooks.assert_cancelled_at = Some(finally_stmt);
            let mut machine = Machine::from_code(code, Limits::default(), 0);
            machine.start_once("Caller", &[], 0, false).unwrap();
            let mut completed = None;
            let mut proved_target = false;
            for _ in 0..1000 {
                match machine
                    .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut hooks)
                    .unwrap()
                {
                    ChunkProgress::More { .. } => {
                        if hooks.observed_queued_cancel && !proved_target {
                            let status = machine
                                .calls
                                .get(0)
                                .unwrap()
                                .env
                                .with_value("task", Span::default(), |value| match value {
                                    Value::Task(task) => Ok(task.status()),
                                    _ => panic!("the caller retains the original target owner"),
                                })
                                .unwrap();
                            assert_eq!(
                                status, "timed_out",
                                "the original cause reaches the target before callee finally exits"
                            );
                            proved_target = true;
                        }
                    }
                    ChunkProgress::Finished { result, .. } => {
                        completed = Some(result);
                        break;
                    }
                    ChunkProgress::Waiting { .. } => panic!("Caller has no source await"),
                }
            }
            assert!(proved_target);
            let result = completed.expect("finite caller cleanup completes");
            tx.send((hooks.observed_queued_cancel, hooks.host.caller_owners.len()))
                .unwrap();
            result.and_then(|flow| flow.into_call_value(Span::default(), "function"))
        })
        .unwrap();
    let witness = rx.recv_timeout(Duration::from_secs(2));
    let result = task.await_result();
    drop(task);
    runtime.cancel_all_and_wait(Duration::from_secs(2)).unwrap();
    let (observed, remaining) = witness.unwrap();
    assert!(
        observed,
        "callee finally must see its caller's queued Task cancelled"
    );
    assert_eq!(remaining, 0);
    assert_eq!(
        result.unwrap_err().runtime_termination().unwrap().reason,
        crate::error::TerminationReason::TimedOut
    );
    let snapshot = runtime.snapshot().unwrap();
    assert_eq!(
        (
            snapshot.active_tasks,
            snapshot.queued_tasks,
            snapshot.registered_tasks,
            snapshot.wait_edges
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(snapshot.finished_tasks, 2);
}
