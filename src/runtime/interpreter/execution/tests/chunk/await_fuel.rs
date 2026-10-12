use super::*;
use std::{sync::mpsc, time::Duration};

const SOURCE: &str = r#"
async fn Child(): int! { return ok(7) }
async fn Entry(): int! {
    target = Child()
    return ok((await target)?)
}
"#;

const FUEL_ERROR: &str = "internal execution prototype test fuel exhausted";

struct FuelHooks {
    trace: TraceHooks,
    observed_scope: bool,
    observed_call: bool,
}

impl ExecutionHooks for FuelHooks {
    fn host(&mut self) -> &mut Interpreter {
        self.trace.host()
    }

    fn tick(&mut self, span: Span) -> KuResult<()> {
        self.trace.tick(span)
    }

    fn finish_scope(
        &mut self,
        env: &mut Env,
        result: KuResult<Flow>,
        span: Span,
        pop: bool,
    ) -> KuResult<Flow> {
        assert!(matches!(&result, Err(error) if error.message == FUEL_ERROR));
        // This callback runs before the real outer Env is popped. The sole
        // target owner must already have left AwaitResume, rather than relying
        // on Machine Drop after this function has unwound.
        assert!(pop);
        assert!(env.contains("target"));
        assert!(env
            .with_value("target", span, |value| Ok(matches!(value, Value::Null)))
            .unwrap());
        assert!(env.all_owned_tasks(span).unwrap().is_empty());
        let snapshot = self
            .trace
            .host
            .task_runtime
            .as_ref()
            .unwrap()
            .snapshot()
            .unwrap();
        assert_eq!((snapshot.wait_edges, snapshot.waiting_tasks), (0, 0));
        assert_eq!((snapshot.cancelled_tasks, snapshot.pending_tasks), (1, 0));
        assert_eq!((snapshot.active_tasks, snapshot.queued_tasks), (2, 1));
        assert_eq!(snapshot.finished_tasks, 0);
        self.observed_scope = true;
        self.trace.finish_scope(env, result, span, pop)
    }

    fn finish_call(
        &mut self,
        env: Env,
        leases: Vec<BorrowLease>,
        result: KuResult<Flow>,
        span: Span,
        kind: CallKind,
    ) -> KuResult<Flow> {
        assert!(self.observed_scope);
        assert!(matches!(&result, Err(error) if error.message == FUEL_ERROR));
        let snapshot = self
            .trace
            .host
            .task_runtime
            .as_ref()
            .unwrap()
            .snapshot()
            .unwrap();
        assert_eq!((snapshot.wait_edges, snapshot.cancelled_tasks), (0, 1));
        assert_eq!((snapshot.queued_tasks, snapshot.finished_tasks), (1, 0));
        self.observed_call = true;
        self.trace.finish_call(env, leases, result, span, kind)
    }

    fn finish_arguments(
        &mut self,
        env: &mut Env,
        leases: Vec<BorrowLease>,
        values: Vec<Value>,
        result: KuResult<Flow>,
        span: Span,
    ) -> KuResult<Flow> {
        self.trace
            .finish_arguments(env, leases, values, result, span)
    }

    fn prepare_finally(
        &mut self,
        env: &Env,
        primary: &KuResult<Flow>,
        span: Span,
    ) -> cancellation::FinallyMode {
        self.trace.prepare_finally(env, primary, span)
    }

    fn finish_finally(
        &mut self,
        primary: KuResult<Flow>,
        replacement: KuResult<Flow>,
        mode: cancellation::FinallyMode,
        span: Span,
    ) -> KuResult<Flow> {
        self.trace.finish_finally(primary, replacement, mode, span)
    }
}

#[test]
fn waiting_fuel_failure_releases_real_target_before_outer_env_and_retires_queue() {
    let source = functions(SOURCE);
    let code =
        CodeHandle::compile_claimed_async_entry(&source, &source["Entry"], Limits::default())
            .unwrap();
    let runtime = TaskRuntime::with_limits(1, 1024, 0, 0, 1024);
    let worker_runtime = runtime.clone();
    let (tx, rx) = mpsc::sync_channel(1);
    // An ordinary callback holds the sole worker throughout both machine
    // phases. No host await/helper can steal its real queued Child beforehand.
    let actor = runtime
        .spawn(move || {
            let mut hooks = FuelHooks {
                trace: TraceHooks::new(),
                observed_scope: false,
                observed_call: false,
            };
            result_entry::install(&mut hooks.trace.host, &source);
            hooks.trace.host.task_runtime = Some(worker_runtime.clone());
            hooks.trace.host.async_execution = true;
            let mut machine = Machine::from_code(code, Limits::default(), 0);
            machine.start_once("Entry", &[], 0, true).unwrap();
            let mut waited = false;
            for _ in 0..100_000 {
                match machine
                    .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut hooks)
                    .unwrap()
                {
                    ChunkProgress::More { popped } => assert_eq!(popped, 1),
                    ChunkProgress::Waiting { popped } => {
                        assert_eq!(popped, 1);
                        waited = true;
                        break;
                    }
                    ChunkProgress::Finished { .. } => {
                        panic!("real queued Child must make await Pending")
                    }
                }
            }
            assert!(
                waited,
                "finite source must reach its actual await checkpoint"
            );
            assert_eq!(machine.state, MachineState::Waiting);
            assert!(machine.awaiting.is_some());
            assert!(matches!(
                machine.work.get(machine.work.len() - 1),
                Some(Work::AwaitResume)
            ));
            assert_eq!(machine.values.len(), 0);
            let env = &machine.calls.get(machine.calls.len() - 1).unwrap().env;
            assert!(env
                .with_value("target", Span::default(), |value| Ok(matches!(
                    value,
                    Value::Null
                )))
                .unwrap());
            assert!(env.all_owned_tasks(Span::default()).unwrap().is_empty());
            let before = worker_runtime.snapshot().unwrap();
            assert_eq!(
                (
                    before.active_tasks,
                    before.queued_tasks,
                    before.registered_tasks
                ),
                (2, 1, 2)
            );
            assert_eq!(
                (
                    before.wait_edges,
                    before.waiting_tasks,
                    before.pending_tasks
                ),
                (1, 1, 1)
            );
            assert_eq!((before.cancelled_tasks, before.finished_tasks), (0, 0));
            // A real suspended invocation cannot be restarted or prepared for
            // reuse. Rejection must preserve both the owner and wait edge.
            let invocation = invocation_snapshot(&machine);
            let ticks = hooks.trace.ticks.len();
            assert!(machine.start_once("Entry", &[], 0, false).is_err());
            assert!(machine.prepare_next_run().is_err());
            assert_eq!(invocation_snapshot(&machine), invocation);
            assert_eq!(hooks.trace.ticks.len(), ticks);
            assert!(machine.awaiting.is_some());
            let rejected = worker_runtime.snapshot().unwrap();
            assert_eq!(
                (
                    rejected.queued_tasks,
                    rejected.wait_edges,
                    rejected.waiting_tasks
                ),
                (1, 1, 1)
            );
            assert_eq!((rejected.cancelled_tasks, rejected.finished_tasks), (0, 0));

            machine.remaining_dispatches = 0;
            let mut outcome = None;
            for _ in 0..100_000 {
                match machine
                    .dispatch_chunk(NonZeroUsize::new(1).unwrap(), &mut hooks)
                    .unwrap()
                {
                    ChunkProgress::More { popped } => assert_eq!(popped, 1),
                    ChunkProgress::Waiting { .. } => {
                        panic!("fuel failure must retire its retained ticket")
                    }
                    ChunkProgress::Finished { result, .. } => {
                        outcome = Some(result);
                        break;
                    }
                }
            }
            let Err(error) = outcome.expect("finite error unwind must finish") else {
                panic!("fuel exhaustion must preserve its original error");
            };
            assert_eq!(error.message, FUEL_ERROR);
            assert_eq!(machine.state, MachineState::Finished);
            assert!(machine.awaiting.is_none());
            assert!(hooks.observed_scope && hooks.observed_call);
            assert!(hooks.trace.host.caller_owners.is_empty());
            assert_eq!(
                hooks.trace.host.steps, ticks,
                "resume/unwind cannot repeat a source tick"
            );
            assert_drained(&machine.metrics, 0);
            tx.send((error.message, hooks.observed_scope, hooks.observed_call))
                .unwrap();
            Ok(Value::Int(99))
        })
        .unwrap();
    let evidence = rx.recv_timeout(Duration::from_secs(2));
    let actor_result = actor.await_result();
    drop(actor);
    let shutdown = runtime.cancel_all_and_wait(Duration::from_secs(2));
    let snapshot = runtime.snapshot().unwrap();
    shutdown.unwrap();
    let (error, scope, call) = evidence.unwrap();
    assert_eq!(error, FUEL_ERROR);
    assert!(scope && call);
    assert_eq!(actor_result.unwrap(), Value::Int(99));
    assert_eq!(
        (
            snapshot.active_tasks,
            snapshot.queued_tasks,
            snapshot.registered_tasks,
            snapshot.wait_edges
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(
        snapshot.finished_tasks, 2,
        "both real queue nodes must retire exactly once"
    );
}
