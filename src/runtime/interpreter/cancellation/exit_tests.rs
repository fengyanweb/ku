use super::exit::ExitState;
use super::*;
use std::sync::{atomic::AtomicBool, Weak};

const TEST_WAIT: Duration = Duration::from_secs(3);

struct ReleaseGate(Option<SyncSender<()>>);

impl ReleaseGate {
    fn pair() -> (Self, Receiver<()>) {
        let (tx, rx) = mpsc::sync_channel(1);
        (Self(Some(tx)), rx)
    }

    fn release(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.try_send(());
        }
    }
}

impl Drop for ReleaseGate {
    fn drop(&mut self) {
        self.release();
    }
}

fn drain(runtime: &TaskRuntime, finished: usize) {
    runtime.cancel_all_and_wait(Duration::from_secs(1)).unwrap();
    let snapshot = runtime.snapshot().unwrap();
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.registered_tasks, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(snapshot.wait_edges, 0);
    assert_eq!(snapshot.finished_tasks, finished);
}

fn listener_payload() -> (Value, Weak<HttpListenerLease>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let id = http_listener_registry::insert(listener).unwrap();
    let lease = HttpListenerLease::new(id);
    let observer = Arc::downgrade(&lease);
    (Value::HttpListenerLease(lease), observer)
}

fn wait_for_completed(task: &crate::runtime::task::TaskHandle) {
    let deadline = Instant::now() + TEST_WAIT;
    while task.status() != "completed" {
        assert!(
            Instant::now() < deadline,
            "real worker did not publish result"
        );
        thread::yield_now();
    }
}

#[test]
fn foreign_runtime_same_number_preserves_both_real_cleanup_obligations() {
    let _execution = ExecutionTerminationGuard::enter();
    let left = TaskRuntime::with_limits(1, 2, 0, 0, 4);
    let right = TaskRuntime::with_limits(1, 2, 0, 0, 4);
    let (mut left_gate, left_wait) = ReleaseGate::pair();
    let (mut right_gate, right_wait) = ReleaseGate::pair();
    let (started, ready) = mpsc::sync_channel(2);
    let (observed, cancellation) = mpsc::sync_channel(2);
    let left_started = started.clone();
    let left_observed = observed.clone();
    let left_task = left
        .spawn(move || {
            left_started.send(()).unwrap();
            left_wait
                .recv_timeout(TEST_WAIT)
                .expect("release left real worker");
            let _ = left_observed.send(current_task_cancellation());
            Ok(Value::Null)
        })
        .unwrap();
    let right_task = right
        .spawn(move || {
            started.send(()).unwrap();
            right_wait
                .recv_timeout(TEST_WAIT)
                .expect("release right real worker");
            let _ = observed.send(current_task_cancellation());
            Ok(Value::Null)
        })
        .unwrap();
    ready.recv_timeout(TEST_WAIT).unwrap();
    ready.recv_timeout(TEST_WAIT).unwrap();
    assert_eq!(left_task.id(), right_task.id());
    assert_ne!(
        left_task.internal_identity_key(),
        right_task.internal_identity_key()
    );
    let span = Span::default();
    let mut env = Env::new();
    env.define_owned("left".into(), Value::Task(left_task), false, span)
        .unwrap();
    env.define_owned("right".into(), Value::Task(right_task), false, span)
        .unwrap();
    let (handles, collected) = call_owned_tasks(Some(&env), &[], &[], span);
    collected.unwrap();
    assert_eq!(
        handles.len(),
        2,
        "both actual states survive internal deduplication"
    );
    let context = CancellationContext::new(TerminationReason::TimedOut);
    let mut interpreter = Interpreter::new();
    interpreter.task_runtime = Some(left.clone());
    interpreter.cancel_visible_children(&env, context, span);
    left_gate.release();
    right_gate.release();
    assert_eq!(cancellation.recv_timeout(TEST_WAIT).unwrap(), Some(context));
    assert_eq!(cancellation.recv_timeout(TEST_WAIT).unwrap(), Some(context));
    for task in &handles {
        task.release_scope_owner(context);
    }
    drop((handles, env));
    drain(&left, 1);
    drain(&right, 1);
    assert_eq!(current_cleanup_context(), None);
    assert_eq!(crate::runtime::task::current_execution_termination(), None);
}

type ExitEvent = (&'static str, Option<RuntimeTermination>);

#[test]
fn actual_retired_child_does_not_make_an_expired_budgeted_exit_successful() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 1, 0, 0, 1);
    let task = runtime.spawn(|| Ok(Value::Null)).unwrap();
    assert_eq!(task.await_result().unwrap(), Value::Null);
    let deadline = Instant::now() + TEST_WAIT;
    while runtime.snapshot().unwrap().finished_tasks != 1 {
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    let span = Span::default();
    let mut env = Env::new();
    env.push_scope();
    env.define_owned("child".into(), Value::Task(task), false, span)
        .unwrap();
    let handles = env.current_scope_owned_tasks(span).unwrap();
    let mut witnesses = Vec::with_capacity(handles.len());
    for handle in &handles {
        witnesses.push(handle.retirement_witness());
    }
    let expired = CancellationContext {
        reason: TerminationReason::Cancelled,
        cleanup_deadline: Instant::now() - Duration::from_millis(1),
    };
    let mut interpreter = Interpreter::new();
    interpreter.task_runtime = Some(runtime.clone());
    let mut flow = Flow::returned(Value::Int(9));
    flow.remember_cleanup(expired);
    let mut state = ExitState::new_with_retirement(
        &mut interpreter,
        Ok(flow),
        span,
        handles,
        Ok(()),
        witnesses,
    );
    state.begin();
    state.wait_terminal(&mut interpreter, &env);
    state.release_owners(&mut interpreter);
    state.drop_handles(&mut interpreter);
    state.teardown(&mut interpreter, &mut env, Env::pop_scope);
    assert!(!env.contains("child"));
    assert_eq!(state.phase(), ExitPhase::WaitRetirement);
    assert_eq!(
        state.poll_retirement(&interpreter, Instant::now()),
        TerminalProgress::Settled
    );
    assert_eq!(state.phase(), ExitPhase::DeadlineCheck);
    assert_eq!(state.retirement_capacity(), 0);
    state.deadline_check(&mut interpreter);
    state.arbitrate(&mut interpreter);
    assert_eq!(state.phase(), ExitPhase::Discard);
    state.discard(&mut interpreter);
    let error = state
        .into_result()
        .err()
        .expect("expired exit must remain a failure");
    assert_eq!(error.code.as_deref(), Some("shutdown_timeout"));
    assert_eq!(error.scope_cleanup(), Some(expired));
    drain(&runtime, 1);
    assert_eq!(current_cleanup_context(), None);
    assert_eq!(crate::runtime::task::current_execution_termination(), None);
}

struct ExitWitness {
    cleanup: RuntimeTermination,
    payload: Value,
    collections: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
    events: Arc<Mutex<Vec<ExitEvent>>>,
}

impl ScopeExit for ExitWitness {
    fn cleanup_context(&self) -> Option<RuntimeTermination> {
        Some(self.cleanup)
    }

    fn remember_cleanup(&mut self, context: RuntimeTermination) {
        self.cleanup.cleanup_deadline = self.cleanup.cleanup_deadline.min(context.cleanup_deadline);
    }

    fn collect_tasks(
        &self,
        tasks: &mut Vec<crate::runtime::task::TaskHandle>,
        span: Span,
    ) -> KuResult<()> {
        self.collections.fetch_add(1, Ordering::AcqRel);
        self.payload.collect_owned_tasks(tasks, span)
    }
}

impl Drop for ExitWitness {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::AcqRel);
        self.events
            .lock()
            .unwrap()
            .push(("outcome_drop", current_cleanup_context()));
    }
}

#[test]
fn owned_exit_preserves_metadata_and_tls_causes_across_phases_at_one_deadline() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::new();
    let (payload, owner_payload_observer) = listener_payload();
    let child = runtime.spawn(move || Ok(payload)).unwrap();
    wait_for_completed(&child);
    let child_id = child.id();
    let span = Span::default();
    let mut env = Env::new();
    env.define_owned("text".into(), Value::String("original".into()), true, span)
        .unwrap();
    env.push_scope();
    env.define_owned("child".into(), Value::Task(child), false, span)
        .unwrap();
    let readonly = env.capture(&HashSet::from(["child".into()]));
    assert!(readonly.all_owned_tasks(span).unwrap().is_empty());
    let handles = env.current_scope_owned_tasks(span).unwrap();
    let (view, restoring_lease) = env.borrow("text", span).unwrap();
    drop(view);
    let (outgoing, outgoing_payload_observer) = listener_payload();
    let collections = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let now = Instant::now();
    let carried = RuntimeTermination {
        reason: TerminationReason::Cancelled,
        cleanup_deadline: now + TEST_WAIT,
    };
    let ambient = RuntimeTermination {
        reason: TerminationReason::TimedOut,
        cleanup_deadline: now + Duration::from_secs(2),
    };
    let expected_initial_meta = RuntimeTermination {
        cleanup_deadline: ambient.cleanup_deadline,
        ..carried
    };
    let mut interpreter = Interpreter::new();
    interpreter.task_runtime = Some(runtime.clone());
    let mut state = {
        let _ambient = CleanupGuard::enter(ambient);
        let mut state = ExitState::new(
            &mut interpreter,
            Ok(ExitWitness {
                cleanup: carried,
                payload: outgoing,
                collections: Arc::clone(&collections),
                drops: Arc::clone(&drops),
                events: Arc::clone(&events),
            }),
            span,
            handles,
            Ok(()),
        );
        {
            let _begin = state.phase_guard();
            state.begin();
        }
        state
    };
    let parked_context = state.context();
    let parked_tls = current_cleanup_context();
    let owned_before_wait = owner_payload_observer.upgrade().is_some();
    state.wait_terminal(&mut interpreter, &env);
    let owned_after_wait = owner_payload_observer.upgrade().is_some();
    let wait_tls = current_cleanup_context();
    state.release_owners(&mut interpreter);
    let owned_after_release = owner_payload_observer.upgrade().is_some();
    let release_tls = current_cleanup_context();
    state.drop_handles(&mut interpreter);
    let handles_tls = current_cleanup_context();
    let outgoing_before_teardown = outgoing_payload_observer.upgrade().is_some();
    let expired = Instant::now()
        .checked_sub(Duration::from_millis(1))
        .unwrap();
    {
        let _teardown = state.phase_guard();
        events
            .lock()
            .unwrap()
            .push(("teardown", current_cleanup_context()));
        // Real owners stay with the synchronous adapter, not in ExitState.
        drop(restoring_lease);
        env.pop_scope();
        let tightening = CleanupGuard::enter(RuntimeTermination {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: expired,
        });
        drop(tightening);
        state.finish_teardown(&mut interpreter);
    }
    let teardown_tls = current_cleanup_context();
    let outgoing_after_teardown = outgoing_payload_observer.upgrade().is_some();
    state.deadline_check(&mut interpreter);
    let outgoing_after_deadline_check = outgoing_payload_observer.upgrade().is_some();
    state.arbitrate(&mut interpreter);
    let outgoing_before_discard = outgoing_payload_observer.upgrade().is_some();
    state.discard(&mut interpreter);
    let outcome = state.into_result();
    let outgoing_after_discard = outgoing_payload_observer.upgrade().is_some();
    let readonly_identity = readonly
        .with_value("child", span, |value| {
            Ok(match value {
                Value::Task(task) => Some((task.id(), task.status())),
                _ => None,
            })
        })
        .unwrap();
    let restored_text = env.get_owning("text", span);
    let writable_after_restore = env.assign("text", Value::String("healthy".into()), span);
    drain(&runtime, 1);

    assert_eq!(parked_context, Some(expected_initial_meta));
    assert_eq!(
        [parked_tls, wait_tls, release_tls, handles_tls, teardown_tls],
        [None; 5]
    );
    assert!(owned_before_wait);
    assert!(
        owned_after_wait,
        "terminal observation released the payload too early"
    );
    assert!(
        !owned_after_release,
        "readonly shared cell retained scope-released payload"
    );
    assert_eq!(readonly_identity, Some((child_id, "completed")));
    assert!(!env.contains("child"));
    assert!(outgoing_before_teardown);
    assert!(outgoing_after_teardown);
    assert!(outgoing_after_deadline_check);
    assert!(outgoing_before_discard);
    assert!(!outgoing_after_discard);
    assert_eq!(collections.load(Ordering::Acquire), 1);
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert_eq!(
        *events.lock().unwrap(),
        vec![
            ("teardown", Some(ambient)),
            (
                "outcome_drop",
                Some(RuntimeTermination {
                    cleanup_deadline: expired,
                    ..ambient
                })
            ),
        ]
    );
    let error = outcome
        .err()
        .expect("post-teardown expiry rejects the outgoing value");
    assert_eq!(error.code.as_deref(), Some("shutdown_timeout"));
    assert_eq!(
        error.scope_cleanup(),
        Some(RuntimeTermination {
            cleanup_deadline: expired,
            ..carried
        })
    );
    assert!(error.runtime_termination().is_none());
    assert_eq!(restored_text.unwrap(), Value::String("original".into()));
    writable_after_restore.unwrap();
    assert!(interpreter.termination.is_none());
    assert!(current_cleanup_context().is_none());
    assert!(crate::runtime::task::current_execution_termination().is_none());
}

#[test]
fn known_real_owner_collection_error_is_first_cleanup_error_and_cannot_replace_typed_primary() {
    for typed_primary in [false, true] {
        let execution = ExecutionTerminationGuard::enter();
        let runtime = TaskRuntime::new();
        let (mut release, release_rx) = ReleaseGate::pair();
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (observed_tx, observed_rx) = mpsc::sync_channel(1);
        let child = runtime
            .spawn(move || {
                started_tx.send(thread::current().id()).unwrap();
                let released = release_rx.recv_timeout(TEST_WAIT).is_ok();
                observed_tx
                    .send((released, current_task_cancellation()))
                    .unwrap();
                Ok(Value::Null)
            })
            .unwrap();
        let worker = started_rx
            .recv_timeout(TEST_WAIT)
            .expect("known owner running");
        let span = Span::default();
        let mut env = Env::new();
        env.push_scope();
        env.define_owned("child".into(), Value::Task(child), false, span)
            .unwrap();
        // These are actual collected proxies, while env retains the real owner.
        // The supplied error exercises the adapter's existing partial-collection
        // contract, not discard_flow's separate collect_tasks failure policy.
        let handles = env.current_scope_owned_tasks(span).unwrap();
        let context = CancellationContext {
            reason: if typed_primary {
                TerminationReason::TimedOut
            } else {
                TerminationReason::Cancelled
            },
            cleanup_deadline: Instant::now()
                .checked_sub(Duration::from_millis(1))
                .unwrap(),
        };
        let primary: KuResult<Flow> = if typed_primary {
            let mut error = KuError::termination(context, span);
            error.message = "original typed primary survives collection and wait failures".into();
            Err(error)
        } else {
            Ok(Flow::Return {
                value: Value::String("outgoing unique value".into()),
                cleanup: Some(context),
            })
        };
        let original_code = primary.as_ref().err().and_then(|error| error.code.clone());
        let mut interpreter = Interpreter::new();
        interpreter.task_runtime = Some(runtime.clone());
        let collection = Err(KuError::structured(
            crate::error::KuErrorKind::Runtime,
            "fixture.collection",
            "first_sentinel",
            "already-known owners survive a later collection error",
            span,
        ));
        let mut teardown_context = None;
        let mut status_before_teardown = None;
        let outcome = interpreter.finish_owned_resources(
            &mut env,
            primary,
            span,
            handles,
            collection,
            |env| {
                teardown_context = current_cleanup_context();
                status_before_teardown = env
                    .with_value("child", span, |value| {
                        Ok(match value {
                            Value::Task(task) => Some(task.status()),
                            _ => None,
                        })
                    })
                    .unwrap();
                env.pop_scope();
            },
        );
        let suppressed = runtime.snapshot().unwrap().suppressed_cleanup_outcomes;
        let restored_tls = current_cleanup_context();
        let latched_reason = interpreter.termination.map(|context| context.reason);
        // The real execution boundary restores a typed primary's TLS before an
        // independent final drain; that drain must not supply this observation.
        drop(execution);
        release.release();
        let observation = observed_rx.recv_timeout(TEST_WAIT);
        drain(&runtime, 1);

        assert_ne!(worker, thread::current().id());
        let (released, child_context) = observation.unwrap();
        assert!(released, "child exhausted its finite containment wait");
        assert_eq!(child_context, Some(context));
        assert_eq!(teardown_context, Some(context));
        assert_eq!(status_before_teardown, Some("cancelling"));
        assert!(!env.contains("child"));
        assert_eq!(restored_tls, None);
        let error = outcome
            .err()
            .expect("collection error cannot become success");
        assert_eq!(error.scope_cleanup(), Some(context));
        if typed_primary {
            assert_eq!(error.code, original_code);
            assert_eq!(
                error.message,
                "original typed primary survives collection and wait failures"
            );
            assert_eq!(error.runtime_termination(), Some(context));
            assert_eq!(latched_reason, Some(context.reason));
            assert_eq!(suppressed, 2);
        } else {
            assert_eq!(error.domain.as_deref(), Some("fixture.collection"));
            assert_eq!(error.code.as_deref(), Some("first_sentinel"));
            assert!(error.runtime_termination().is_none());
            assert_eq!(latched_reason, None);
            assert_eq!(suppressed, 1);
        }
        assert!(current_cleanup_context().is_none());
        assert!(crate::runtime::task::current_execution_termination().is_none());
    }
}

// Occupy the existing runtime's actual workers so queued-capture destruction
// goes through the cancellation path without introducing a test-only runtime.
struct HeldWorkers {
    releases: Vec<ReleaseGate>,
    handles: Vec<crate::runtime::task::TaskHandle>,
}

impl HeldWorkers {
    fn start(runtime: &TaskRuntime) -> Self {
        let count = runtime.snapshot().unwrap().task_workers;
        assert_ne!(count, 0, "fixture needs real workers");
        let mut held = Self {
            releases: Vec::new(),
            handles: Vec::new(),
        };
        let mut threads = HashSet::new();
        for _ in 0..count {
            let (release, release_rx) = ReleaseGate::pair();
            held.releases.push(release);
            let (started_tx, started_rx) = mpsc::sync_channel(1);
            held.handles.push(
                runtime
                    .spawn(move || {
                        started_tx.send(thread::current().id()).unwrap();
                        release_rx
                            .recv_timeout(TEST_WAIT)
                            .expect("release held worker");
                        Ok(Value::Null)
                    })
                    .unwrap(),
            );
            let id = started_rx
                .recv_timeout(TEST_WAIT)
                .expect("real held worker entered");
            assert_ne!(id, thread::current().id());
            assert!(
                threads.insert(id),
                "one blocked worker cannot start another job"
            );
        }
        held
    }

    fn release(&mut self) {
        for release in &mut self.releases {
            release.release();
        }
    }
}

struct CaptureDrop {
    observed: SyncSender<Option<CancellationContext>>,
    count: Arc<AtomicUsize>,
}

impl Drop for CaptureDrop {
    fn drop(&mut self) {
        self.count.fetch_add(1, Ordering::AcqRel);
        let _ = self.observed.try_send(current_cleanup_context());
    }
}

#[test]
fn teardown_panic_releases_exit_outcome_once_at_stored_deadline_and_restores_real_borrow() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::new();
    let mut held = HeldWorkers::start(&runtime);
    let (drop_tx, drop_rx) = mpsc::sync_channel(1);
    let drops = Arc::new(AtomicUsize::new(0));
    let executed = Arc::new(AtomicBool::new(false));
    let child_executed = Arc::clone(&executed);
    let probe = CaptureDrop {
        observed: drop_tx,
        count: Arc::clone(&drops),
    };
    let child = runtime
        .spawn(move || {
            child_executed.store(true, Ordering::Release);
            drop(probe);
            Ok(Value::Null)
        })
        .unwrap();
    let span = Span::default();
    let mut captures = Env::new();
    captures
        .define_owned("child".into(), Value::Task(child), false, span)
        .unwrap();
    let (resource, payload_observer) = listener_payload();
    captures
        .define_owned("resource".into(), resource, false, span)
        .unwrap();
    let context = CancellationContext {
        reason: TerminationReason::Cancelled,
        cleanup_deadline: Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap(),
    };
    let outcome = Flow::Return {
        value: Value::Function {
            params: Vec::new(),
            param_modes: Vec::new(),
            body: Vec::new(),
            captures,
            self_name: None,
            is_async: false,
        },
        cleanup: Some(context),
    };
    let mut caller = Env::new();
    caller
        .define_owned("text".into(), Value::String("original".into()), true, span)
        .unwrap();
    let (view, lease) = caller.borrow("text", span).unwrap();
    drop(view);
    let leases: Vec<_> = lease.into_iter().collect();
    let mut interpreter = Interpreter::new();
    interpreter.task_runtime = Some(runtime.clone());
    let mut teardown_context = None;
    let tightened = CancellationContext {
        cleanup_deadline: context.cleanup_deadline - Duration::from_millis(1),
        ..context
    };
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // The adapter creates ExitState inside this real production lifetime;
        // its synchronous closure retains sole Env/lease ownership.
        let _ = interpreter.finish_owned_resources(
            &mut caller,
            Ok(outcome),
            span,
            Vec::new(),
            Ok(()),
            |_| {
                let _restoring_leases = leases;
                teardown_context = current_cleanup_context();
                let tighter = CleanupGuard::enter(CancellationContext {
                    reason: TerminationReason::TimedOut,
                    cleanup_deadline: tightened.cleanup_deadline,
                });
                drop(tighter);
                panic!("real synchronous teardown panic while ExitState owns result");
            },
        );
    }));
    let dropped_context = drop_rx.recv_timeout(TEST_WAIT);
    let restored_tls = current_cleanup_context();
    let payload_released = payload_observer.upgrade().is_none();
    let borrowed_value_restored = caller.get_owning("text", span);
    let writable_after_restore = caller.assign("text", Value::String("healthy".into()), span);
    held.release();
    drain(&runtime, held.handles.len() + 1);

    assert!(panic.is_err());
    assert_eq!(teardown_context, Some(context));
    assert_eq!(dropped_context.unwrap(), Some(tightened));
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert!(!executed.load(Ordering::Acquire));
    assert!(
        payload_released,
        "outgoing Function retained a captured resource owner"
    );
    assert_eq!(
        borrowed_value_restored.unwrap(),
        Value::String("original".into())
    );
    writable_after_restore.unwrap();
    assert_eq!(restored_tls, None);
    assert!(interpreter.termination.is_none());
    assert!(crate::runtime::task::current_execution_termination().is_none());
    assert_eq!(
        interpreter
            .exec_block(
                &super::super::finally_tests::body("return 9"),
                &mut caller,
                0
            )
            .unwrap()
            .into_call_value(span, "healthy after exit panic")
            .unwrap(),
        Value::Int(9)
    );
}
