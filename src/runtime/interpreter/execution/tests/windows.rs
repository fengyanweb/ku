use super::*;
use std::sync::{atomic::AtomicBool, Weak};

const FIXTURE_WAIT: Duration = Duration::from_secs(3);
const MAX_CHUNKS: usize = 10_000;
const NATIVE_PANIC: &str = "actual finally tick window panic";

// An old stored guard can restore an already-exited ambient context on RED.
// Isolate that real TLS failure instead of repairing private TLS or polluting
// subsequent tests. Every Machine is created, dispatched, and dropped on this
// one host thread; this is not a worker migration fixture.
fn isolated(case: impl FnOnce() + Send + 'static) {
    let (tx, rx) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(case));
        let _ = tx.send(result);
    });
    let result = rx
        .recv_timeout(Duration::from_secs(15))
        .expect("finite window fixture host must return");
    worker
        .join()
        .expect("window fixture host returned its panic");
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[derive(Clone, Copy)]
enum Action {
    Terminate(RuntimeTermination),
    Tighten(RuntimeTermination),
    TightenAndPanic(RuntimeTermination),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FinallyObservation {
    span: Span,
    protected: Option<RuntimeTermination>,
    tls: Option<RuntimeTermination>,
    primary_return: Option<i64>,
}

struct WindowHooks {
    host: Interpreter,
    actions: Vec<(Span, Action)>,
    ticks: Vec<(Span, Option<RuntimeTermination>)>,
    prepared: Vec<FinallyObservation>,
    finished: Vec<FinallyObservation>,
    markers: Vec<i64>,
    panic_window: Option<RuntimeTermination>,
    panic_after_finally: Option<RuntimeTermination>,
}

impl WindowHooks {
    fn new(actions: Vec<(Span, Action)>) -> Self {
        Self {
            host: Interpreter::new(),
            actions,
            ticks: Vec::new(),
            prepared: Vec::new(),
            finished: Vec::new(),
            markers: Vec::new(),
            panic_window: None,
            panic_after_finally: None,
        }
    }
}

fn observation(
    span: Span,
    mode: cancellation::FinallyMode,
    primary: &KuResult<Flow>,
) -> FinallyObservation {
    FinallyObservation {
        span,
        protected: match mode {
            cancellation::FinallyMode::Protected(context) => Some(context),
            cancellation::FinallyMode::Skip | cancellation::FinallyMode::Ordinary => None,
        },
        tls: current_cleanup_context(),
        primary_return: match primary {
            Ok(Flow::Return {
                value: Value::Int(value),
                ..
            }) => Some(*value),
            _ => None,
        },
    }
}

impl ExecutionHooks for WindowHooks {
    fn host(&mut self) -> &mut Interpreter {
        &mut self.host
    }

    fn tick(&mut self, span: Span) -> KuResult<()> {
        self.ticks.push((span, current_cleanup_context()));
        if self
            .actions
            .first()
            .is_some_and(|(target, _)| *target == span)
        {
            match self.actions.remove(0).1 {
                Action::Terminate(context) => return Err(KuError::termination(context, span)),
                Action::Tighten(context) => drop(CleanupGuard::enter(context)),
                Action::TightenAndPanic(context) => {
                    // This short real guard tightens the active window, then
                    // its Drop returns before the native tick starts unwinding.
                    drop(CleanupGuard::enter(context));
                    self.panic_window = current_cleanup_context();
                    panic!("{NATIVE_PANIC}");
                }
            }
        }
        ExecutionHooks::tick(&mut self.host, span)
    }

    fn finish_scope(
        &mut self,
        env: &mut Env,
        result: KuResult<Flow>,
        span: Span,
        pop: bool,
    ) -> KuResult<Flow> {
        if let Ok(Value::Int(marker)) = env.get("marker", span) {
            self.markers.push(marker);
        }
        ExecutionHooks::finish_scope(&mut self.host, env, result, span, pop)
    }

    fn finish_call(
        &mut self,
        env: Env,
        leases: Vec<BorrowLease>,
        result: KuResult<Flow>,
        span: Span,
        kind: CallKind,
    ) -> KuResult<Flow> {
        ExecutionHooks::finish_call(&mut self.host, env, leases, result, span, kind)
    }

    fn finish_arguments(
        &mut self,
        env: &mut Env,
        leases: Vec<BorrowLease>,
        values: Vec<Value>,
        result: KuResult<Flow>,
        span: Span,
    ) -> KuResult<Flow> {
        ExecutionHooks::finish_arguments(&mut self.host, env, leases, values, result, span)
    }

    fn prepare_finally(
        &mut self,
        env: &Env,
        primary: &KuResult<Flow>,
        span: Span,
    ) -> cancellation::FinallyMode {
        let mode = ExecutionHooks::prepare_finally(&mut self.host, env, primary, span);
        self.prepared.push(observation(span, mode, primary));
        mode
    }

    fn finish_finally(
        &mut self,
        primary: KuResult<Flow>,
        replacement: KuResult<Flow>,
        mode: cancellation::FinallyMode,
        span: Span,
    ) -> KuResult<Flow> {
        self.finished.push(observation(span, mode, &primary));
        let result =
            ExecutionHooks::finish_finally(&mut self.host, primary, replacement, mode, span);
        if matches!(mode, cancellation::FinallyMode::Protected(_)) {
            if let Some(context) = self.panic_after_finally.take() {
                // The real FinallyExit has already popped its frame and the
                // real host has arbitrated. Tighten this still-live window
                // beyond the D that host tick previously latched.
                drop(CleanupGuard::enter(context));
                self.panic_window = current_cleanup_context();
                panic!("{NATIVE_PANIC}");
            }
        }
        result
    }
}

fn checkpoint(
    machine: &mut Machine,
    hooks: &mut WindowHooks,
    reached: impl Fn(&WindowHooks) -> bool,
) {
    for _ in 0..MAX_CHUNKS {
        match machine
            .dispatch_chunk(NonZeroUsize::new(1).unwrap(), hooks)
            .unwrap()
        {
            ChunkProgress::Waiting { .. } => panic!("this scalar fixture has no await"),
            ChunkProgress::More { popped } => {
                assert_eq!(popped, 1);
                if reached(hooks) {
                    return;
                }
            }
            ChunkProgress::Finished { .. } => panic!("real finally checkpoint was not reached"),
        }
    }
    panic!("finite fixture did not reach its real finally checkpoint");
}

fn finish(machine: &mut Machine, hooks: &mut WindowHooks) -> KuResult<Flow> {
    for _ in 0..MAX_CHUNKS {
        match machine
            .dispatch_chunk(NonZeroUsize::new(1).unwrap(), hooks)
            .unwrap()
        {
            ChunkProgress::Waiting { .. } => panic!("this scalar fixture has no await"),
            ChunkProgress::More { popped } => assert_eq!(popped, 1),
            ChunkProgress::Finished { result, .. } => return result,
        }
    }
    panic!("finite finally fixture did not complete");
}

#[test]
fn protected_finally_more_restores_tls_before_the_next_real_dispatch() {
    isolated(|| {
        let _execution = ExecutionTerminationGuard::enter();
        let source = functions(
            "fn Entry(): int { try { while (true) {} } finally { marker = 1 } return 7 }",
        );
        let Stmt::Try { body, .. } = &source["Entry"].body[0] else {
            panic!("real try fixture");
        };
        let original = RuntimeTermination {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: Instant::now() + FIXTURE_WAIT,
        };
        let before = current_cleanup_context();
        let mut hooks = WindowHooks::new(vec![(stmt_span(&body[0]), Action::Terminate(original))]);
        let mut machine = Machine::new(&source, Limits::default(), 0).unwrap();
        machine.start_once("Entry", &[], 0, false).unwrap();
        checkpoint(&mut machine, &mut hooks, |hooks| hooks.prepared.len() == 1);
        let between_chunks = current_cleanup_context();
        let error = finish(&mut machine, &mut hooks)
            .err()
            .expect("protected finite finally retains termination");
        let after = current_cleanup_context();
        assert_drained(&machine.metrics, 0);
        eprintln!(
            "dispatch windows sizes: call={} work={} finally={} protection={} prefix={} machine={} metrics={:?}",
            std::mem::size_of::<CallFrame>(),
            std::mem::size_of::<Work>(),
            std::mem::size_of::<FinallyFrame>(),
            std::mem::size_of::<Protection>(),
            std::mem::size_of::<MachineResources>(),
            std::mem::size_of::<Machine>(),
            machine.metrics
        );
        drop(machine);
        assert_eq!(before, None);
        assert_eq!(
            between_chunks, before,
            "More must return outside its cleanup window"
        );
        assert_eq!(after, before);
        assert_eq!(hooks.prepared[0].protected, Some(original));
        assert!(
            hooks.markers.contains(&1),
            "real finally body must execute on resume"
        );
        assert_eq!(error.runtime_termination().unwrap().reason, original.reason);
    });
}

#[test]
fn resumed_finite_and_empty_finally_keep_metadata_cause_and_shortest_ambient_deadline() {
    for empty in [false, true] {
        isolated(move || {
            let _execution = ExecutionTerminationGuard::enter();
            let mut source = functions(if empty {
                "fn Entry(): int { try { while (true) {} } finally { marker = 0 } return 7 }"
            } else {
                "fn Entry(): int { try { while (true) {} } finally { marker = 1 marker = 2 } return 7 }"
            });
            if empty {
                // The current parser requires a nonempty catch/finally body.
                // Exercise an empty production CodeArena block directly, not
                // a claim that empty finally source syntax is accepted.
                let Stmt::Try { finally_body, .. } = &mut source.get_mut("Entry").unwrap().body[0]
                else {
                    panic!("real try fixture");
                };
                finally_body.clear();
            }
            let Stmt::Try {
                body, finally_body, ..
            } = &source["Entry"].body[0]
            else {
                panic!("real try fixture");
            };
            let now = Instant::now();
            let original = RuntimeTermination {
                reason: TerminationReason::Cancelled,
                cleanup_deadline: now + Duration::from_secs(8),
            };
            let shortened = RuntimeTermination {
                reason: TerminationReason::TimedOut,
                cleanup_deadline: now + FIXTURE_WAIT,
            };
            let ambient = CleanupGuard::enter(RuntimeTermination {
                reason: shortened.reason,
                cleanup_deadline: if empty {
                    shortened.cleanup_deadline
                } else {
                    now + Duration::from_secs(6)
                },
            });
            let mut actions = vec![(stmt_span(&body[0]), Action::Terminate(original))];
            if !empty {
                actions.push((stmt_span(&finally_body[0]), Action::Tighten(shortened)));
            }
            let mut hooks = WindowHooks::new(actions);
            let mut machine = Machine::new(&source, Limits::default(), 0).unwrap();
            machine.start_once("Entry", &[], 0, false).unwrap();
            checkpoint(&mut machine, &mut hooks, |hooks| {
                hooks.prepared.len() == 1 && hooks.actions.is_empty()
            });
            let ambient_after_checkpoint = current_cleanup_context();
            drop(ambient);
            let outside_ambient = current_cleanup_context();
            let error = finish(&mut machine, &mut hooks)
                .err()
                .expect("metadata cancellation survives finally");
            let after = current_cleanup_context();
            assert_drained(&machine.metrics, 0);
            drop(machine);
            assert_eq!(ambient_after_checkpoint, Some(shortened));
            assert_eq!(outside_ambient, None);
            assert_eq!(
                after, None,
                "retired windows must not resurrect an exited ambient guard"
            );
            assert_eq!(hooks.prepared[0].protected.unwrap().reason, original.reason);
            assert_eq!(error.runtime_termination().unwrap().reason, original.reason);
            assert_eq!(
                error.scope_cleanup(),
                Some(RuntimeTermination {
                    reason: original.reason,
                    ..shortened
                }),
                "even an empty finally must carry the shortest actual window into its outcome"
            );
            if !empty {
                assert!(
                    hooks.markers.contains(&2),
                    "the actual resumed finite body must finish"
                );
                assert!(hooks
                    .ticks
                    .contains(&(stmt_span(&finally_body[1]), Some(shortened))));
            }
        });
    }
}

struct ReleaseGate(Option<SyncSender<()>>);

impl ReleaseGate {
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

struct HeldWorkers {
    // Gates retire before their logical Task owners on every ordinary unwind.
    releases: Vec<ReleaseGate>,
    handles: Vec<crate::runtime::task::TaskHandle>,
}

impl HeldWorkers {
    fn start(runtime: &TaskRuntime) -> Self {
        let count = runtime.snapshot().unwrap().task_workers;
        assert_ne!(count, 0);
        let mut held = Self {
            releases: Vec::new(),
            handles: Vec::new(),
        };
        let mut threads = HashSet::new();
        for _ in 0..count {
            let (release_tx, release_rx) = mpsc::sync_channel(1);
            held.releases.push(ReleaseGate(Some(release_tx)));
            let (started_tx, started_rx) = mpsc::sync_channel(1);
            held.handles.push(
                runtime
                    .spawn(move || {
                        started_tx.send(thread::current().id()).unwrap();
                        release_rx
                            .recv_timeout(FIXTURE_WAIT)
                            .expect("release finite worker gate");
                        Ok(Value::Null)
                    })
                    .unwrap(),
            );
            let id = started_rx
                .recv_timeout(FIXTURE_WAIT)
                .expect("real worker entered");
            assert_ne!(id, thread::current().id());
            assert!(
                threads.insert(id),
                "each finite gate must occupy a distinct real worker"
            );
        }
        held
    }

    fn release(&mut self) {
        for gate in &mut self.releases {
            gate.release();
        }
    }
}

struct CaptureDrop {
    observed: SyncSender<Option<RuntimeTermination>>,
    count: Arc<AtomicUsize>,
}

impl Drop for CaptureDrop {
    fn drop(&mut self) {
        self.count.fetch_add(1, Ordering::AcqRel);
        let _ = self.observed.try_send(current_cleanup_context());
    }
}

fn listener_payload() -> (Value, Weak<HttpListenerLease>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let id = http_listener_registry::insert(listener).unwrap();
    let lease = HttpListenerLease::new(id);
    let weak = Arc::downgrade(&lease);
    (Value::HttpListenerLease(lease), weak)
}

const MIXED_FINALLY: &str = r#"
fn Entry(): int {
    marker = 0
    try { return 7 }
    finally {
        marker = 1
        try { while (true) {} }
        finally { marker = marker * 10 + 2 }
    }
    return 99
}
"#;

#[test]
fn finally_tick_panic_restores_tls_with_machine_alive_and_prefix_drop_releases_real_owners() {
    // Exercise the distinct stored TLS cause first: on the old implementation
    // its actual prefix drop RED is observed before any earlier assertion aborts.
    for (has_ambient, after_pop) in [(true, false), (false, false), (true, true), (false, true)] {
        isolated(move || {
            let _execution = ExecutionTerminationGuard::enter();
            let runtime = TaskRuntime::new();
            let mut held = HeldWorkers::start(&runtime);
            let (drop_tx, drop_rx) = mpsc::sync_channel(1);
            let drops = Arc::new(AtomicUsize::new(0));
            let ran = Arc::new(AtomicBool::new(false));
            let child_ran = Arc::clone(&ran);
            let (payload, weak) = listener_payload();
            let captures = (
                CaptureDrop {
                    observed: drop_tx,
                    count: Arc::clone(&drops),
                },
                payload,
            );
            let child = runtime
                .spawn(move || {
                    child_ran.store(true, Ordering::Release);
                    drop(captures);
                    Ok(Value::Null)
                })
                .unwrap();
            assert_eq!(
                child.status(),
                "pending",
                "the actual owner must remain queued"
            );
            let mut captured_env = Env::new();
            captured_env
                .define_owned(
                    "unused_child".into(),
                    Value::Task(child),
                    false,
                    Span::default(),
                )
                .unwrap();
            let source = functions(MIXED_FINALLY);
            let Stmt::Try {
                finally_body: outer,
                ..
            } = &source["Entry"].body[1]
            else {
                panic!("ordinary outer finally");
            };
            let Stmt::Try {
                body,
                finally_body: inner,
                ..
            } = &outer[1]
            else {
                panic!("protected inner finally");
            };
            let now = Instant::now();
            let original = RuntimeTermination {
                reason: TerminationReason::Cancelled,
                cleanup_deadline: now + Duration::from_secs(8),
            };
            let shorter = RuntimeTermination {
                reason: TerminationReason::TimedOut,
                cleanup_deadline: now + FIXTURE_WAIT,
            };
            let expected_drop = RuntimeTermination {
                reason: if has_ambient {
                    shorter.reason
                } else {
                    original.reason
                },
                cleanup_deadline: if after_pop {
                    now + Duration::from_secs(2)
                } else {
                    shorter.cleanup_deadline
                },
            };
            let ambient = has_ambient.then(|| {
                CleanupGuard::enter(RuntimeTermination {
                    reason: shorter.reason,
                    cleanup_deadline: now + Duration::from_secs(6),
                })
            });
            let mut hooks = WindowHooks::new(vec![
                (stmt_span(&body[0]), Action::Terminate(original)),
                (
                    stmt_span(&inner[0]),
                    if after_pop {
                        Action::Tighten(shorter)
                    } else {
                        Action::TightenAndPanic(shorter)
                    },
                ),
            ]);
            hooks.panic_after_finally = after_pop.then_some(expected_drop);
            hooks.host.task_runtime = Some(runtime.clone());
            let mut machine = Machine::new(&source, Limits::default(), 0).unwrap();
            // Actual unique resources in the same kernel's Env owner. General
            // Owned-capture source admission remains outside the scalar slice.
            // Env clones inside enter share these cells, never clone the Task.
            machine.captures = Some(captured_env);
            machine.start_once("Entry", &[], 0, false).unwrap();
            checkpoint(&mut machine, &mut hooks, |hooks| hooks.prepared.len() == 2);
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = finish(&mut machine, &mut hooks);
            }));
            let panic_message = panic.as_ref().err().and_then(|payload| {
                payload
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| payload.downcast_ref::<&str>().copied())
            });
            let immediately_after_panic = current_cleanup_context();
            let live_before = (
                machine.calls.len(),
                machine.work.len(),
                machine.values.len(),
                machine.finally.frames.len(),
                machine.finally.active_protected,
                machine.finally.context(),
                machine.metrics.dispatches,
            );
            assert_eq!(machine.state, MachineState::Aborted);
            assert!(machine
                .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut hooks)
                .is_err());
            assert!(machine.start_once("Entry", &[], 0, false).is_err());
            assert!(machine.prepare_next_run().is_err());
            assert_eq!(
                live_before,
                (
                    machine.calls.len(),
                    machine.work.len(),
                    machine.values.len(),
                    machine.finally.frames.len(),
                    machine.finally.active_protected,
                    machine.finally.context(),
                    machine.metrics.dispatches,
                ),
                "aborted lifecycle rejection cannot resume or discard live owners"
            );
            assert_eq!(current_cleanup_context(), immediately_after_panic);
            let payload_alive_before_drop = weak.upgrade().is_some();
            let drops_before = drops.load(Ordering::Acquire);
            drop(ambient);
            let after_ambient = current_cleanup_context();
            drop(machine);
            let actual_drop = drop_rx.recv_timeout(FIXTURE_WAIT);
            let payload_gone = weak.upgrade().is_none();
            let after_drop = current_cleanup_context();
            held.release();
            let drained = runtime.cancel_all_and_wait(Duration::from_secs(1));
            let snapshot = runtime.snapshot().unwrap();
            drop(held);
            eprintln!("finally panic ambient={has_ambient} after_pop={after_pop} panic={panic_message:?} tls={immediately_after_panic:?}/{after_ambient:?}/{after_drop:?} owner_drop={actual_drop:?} listener_gone={payload_gone} snapshot={snapshot:?}");
            assert_eq!(panic_message, Some(NATIVE_PANIC));
            assert!(hooks.actions.is_empty());
            assert_eq!(hooks.panic_window, Some(expected_drop));
            assert_eq!(
                immediately_after_panic,
                has_ambient.then_some(expected_drop),
                "panic returns outside the dispatch window even while Machine remains alive"
            );
            assert_eq!(after_ambient, None);
            assert!(payload_alive_before_drop);
            assert_eq!(drops_before, 0);
            assert_eq!(
                actual_drop.unwrap(),
                Some(expected_drop),
                "prefix owner Drop retains TLS first cause and the shortest saved D"
            );
            assert_eq!(drops.load(Ordering::Acquire), 1);
            assert!(!ran.load(Ordering::Acquire));
            assert!(payload_gone);
            assert_eq!(after_drop, None);
            drained.unwrap();
            assert_eq!(snapshot.active_tasks, 0);
            assert_eq!(snapshot.registered_tasks, 0);
            assert_eq!(snapshot.queued_tasks, 0);
            assert_eq!(snapshot.wait_edges, 0);
        });
    }
}

#[test]
fn ordinary_outer_finally_arbitrates_after_inner_protected_window_has_retired() {
    isolated(|| {
        let _execution = ExecutionTerminationGuard::enter();
        let source = functions(MIXED_FINALLY);
        let Stmt::Try {
            finally_body: outer,
            span: outer_span,
            ..
        } = &source["Entry"].body[1]
        else {
            panic!("ordinary outer finally");
        };
        let Stmt::Try {
            body,
            span: inner_span,
            ..
        } = &outer[1]
        else {
            panic!("protected inner finally");
        };
        let original = RuntimeTermination {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: Instant::now() + FIXTURE_WAIT,
        };
        let mut hooks = WindowHooks::new(vec![(stmt_span(&body[0]), Action::Terminate(original))]);
        let mut machine = Machine::new(&source, Limits::default(), 0).unwrap();
        machine.start_once("Entry", &[], 0, false).unwrap();
        let error = finish(&mut machine, &mut hooks)
            .err()
            .expect("inner cancellation overrides ordinary outer return");
        let after = current_cleanup_context();
        assert_drained(&machine.metrics, 0);
        drop(machine);
        assert_eq!(hooks.prepared.len(), 2);
        assert_eq!(hooks.prepared[0].span, *outer_span);
        assert_eq!(hooks.prepared[0].protected, None);
        assert_eq!(hooks.prepared[0].primary_return, Some(7));
        assert_eq!(hooks.prepared[1].span, *inner_span);
        assert_eq!(hooks.prepared[1].protected, Some(original));
        assert_eq!(hooks.finished.len(), 2);
        assert_eq!(hooks.finished[0].span, *inner_span);
        assert_eq!(hooks.finished[0].protected, Some(original));
        assert_eq!(hooks.finished[0].tls, Some(original));
        assert_eq!(hooks.finished[1].span, *outer_span);
        assert_eq!(hooks.finished[1].protected, None);
        assert_eq!(
            hooks.finished[1].tls, None,
            "inner cleanup must not blanket-protect the saved ordinary outer primary"
        );
        assert_eq!(hooks.finished[1].primary_return, Some(7));
        assert!(hooks.markers.contains(&12));
        assert_eq!(error.runtime_termination().unwrap().reason, original.reason);
        assert_eq!(after, None);
    });
}
