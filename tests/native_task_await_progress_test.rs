//! Await is a storage boundary, not an unconditional scheduling boundary.
//! A regressed verifier is tested with real bounded admission failure. Source
//! while probes observe two returned polls before requesting real cancellation.
#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;
#[path = "support/native_task_ledger.rs"]
mod native_task_ledger;

use ku::{
    backend::c,
    checker::Checker,
    ir::{self, task::*, IrType},
    lexer::Lexer,
    parser::Parser,
};
use native_harness::{
    compile_harness, run_bounded, TempDir, NATIVE_THREAD_LIFECYCLE_HARNESS, RUN_LIMITS, RUN_TIMEOUT,
};
use std::{fs, process::Command};

fn value(ty: IrType) -> TaskSlot {
    TaskSlot {
        ty: TaskSlotType::Value {
            ty,
            borrowed: false,
        },
    }
}
fn result(ty: IrType) -> IrType {
    IrType::Result(Box::new(ty))
}
fn program(suspend: bool) -> TaskProgram {
    TaskProgram {
        functions: vec![
            TaskFunction {
                id: TaskFunctionId(0),
                name: "Parent".into(),
                parameters: vec![],
                entry: StateId(0),
                result: result(IrType::Null),
                slots: vec![
                    TaskSlot {
                        ty: TaskSlotType::Task {
                            result: result(IrType::Int),
                        },
                    },
                    value(result(IrType::Int)),
                ],
                states: vec![
                    TaskState {
                        operations: vec![TaskOp::Start {
                            dst: SlotId(0),
                            function: TaskFunctionId(1),
                            arguments: vec![],
                        }],
                        terminator: TaskTerminator::Jump { target: StateId(1) },
                    },
                    TaskState {
                        operations: vec![],
                        terminator: TaskTerminator::Await {
                            task: SlotId(0),
                            dst: SlotId(1),
                            ready: StateId(2),
                            cleanup: StateId(3),
                        },
                    },
                    TaskState {
                        operations: vec![TaskOp::Drop { slot: SlotId(1) }],
                        terminator: if suspend {
                            TaskTerminator::Suspend {
                                resume: StateId(0),
                                cleanup: StateId(3),
                            }
                        } else {
                            TaskTerminator::Jump { target: StateId(0) }
                        },
                    },
                    TaskState {
                        operations: vec![],
                        terminator: TaskTerminator::Terminate,
                    },
                ],
            },
            TaskFunction {
                id: TaskFunctionId(1),
                name: "Leaf".into(),
                parameters: vec![],
                entry: StateId(0),
                result: result(IrType::Int),
                slots: vec![value(result(IrType::Int))],
                states: vec![TaskState {
                    operations: vec![TaskOp::Init {
                        dst: SlotId(0),
                        value: TaskConstant::Ok(Box::new(TaskConstant::Int(7))),
                    }],
                    terminator: TaskTerminator::Complete { value: SlotId(0) },
                }],
            },
        ],
    }
}
fn generate(tasks: &TaskProgram) -> ku::error::KuResult<String> {
    let ast = Parser::new(Lexer::new("fn main() {}").lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    c::generate_task_frame_c_source(&ir::lower_program(&ast).unwrap(), tasks)
}
fn replace_once(source: String, anchor: &str, replacement: &str) -> String {
    assert_eq!(source.matches(anchor).count(), 1, "progress hook {anchor}");
    source.replacen(anchor, replacement, 1)
}
fn run_progress(mut generated: String, label: &str) {
    let instrumentation = format!("{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{}\n{}\n{}\nstatic void fixture_resume(void*);\nstatic void fixture_inline(void*,uint32_t);\nstatic void fixture_awaited(void*);\n",
        native_task_ledger::LEDGER_LOCK, native_allocation_harness::ALLOCATION_HOOK, native_task_ledger::LOCKED_ALLOCATIONS);
    generated = replace_once(
        generated,
        "typedef struct KuString {",
        &format!("{instrumentation}typedef struct KuString {{"),
    );
    let resume = "static uint32_t ku_task_0_resume(void* raw) {";
    generated = replace_once(
        generated,
        resume,
        &format!("{resume}\n  fixture_resume(raw);"),
    );
    let inline = "output->tag=KU_TASK_VALUE_INLINE_FAILED; output->result_kind=1u; output->function_id=1u; output->rejection_code=status;";
    generated = replace_once(
        generated,
        inline,
        &format!("{inline}\n    fixture_inline(output,status);"),
    );
    let awaited = "  if (received.exit_class==KU_TASK_EXIT_RUNTIME_FAILURE) {";
    generated = replace_once(
        generated,
        awaited,
        &format!("  fixture_awaited(&received);\n{awaited}"),
    );
    generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_original_main(void) {",
    );
    assert!(!generated.contains("run_source") && !generated.contains("const SOURCE"));
    generated.push_str(C_MAIN);
    run_generated(
        generated,
        label,
        "await-progress resumes=2 returned_polls=1 ledger=0\n",
    );
}
fn run_generated(generated: String, label: &str, expected: &str) {
    let directory = TempDir::new(label);
    let source = directory.path().join("program.c");
    fs::write(&source, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &source, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires real native progress execution"
        );
        eprintln!("no compiler: {label} native progress execution skipped");
        return;
    };
    fs::remove_file(source).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("progress probe must reach its event and cancel under existing bounds");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{label}: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        expected
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn native_task_await_only_cycle_rejects_before_artifact_or_exposes_same_poll_reentry() {
    match generate(&program(false)) {
        Err(error) => assert!(
            error.message.contains("cycle without suspension"),
            "{error}"
        ),
        Ok(generated) => {
            // A regressed verifier must not pass silently. No synthetic result:
            // capacity 1 forces real INLINE_FAILED and the second real Start
            // pauses while main selects real cancellation, then clears ledger.
            run_progress(generated, "native-await-progress-regression");
            panic!("Await-only cycle was accepted; verifier must reject before C generation");
        }
    }
}
#[test]
fn native_task_explicit_suspend_returns_poll_before_second_inline_failed_start() {
    let tasks = program(true);
    verify_and_plan(&tasks, TaskLimits::default()).unwrap();
    run_progress(generate(&tasks).unwrap(), "native-await-progress-suspend");
}

#[test]
fn native_task_source_while_returns_two_polls_before_real_cancellation() {
    let cases = [
        (
            "async fn main(): null! { while (true) {} return ok(null) }",
            "native-source-while-empty-cancel",
            false,
        ),
        (
            "async fn main(): null! { index = 0 while (index < 4) { index = index + 1 } return ok(null) }",
            "native-source-while-copy-cancel",
            true,
        ),
    ];
    for (source, label, has_counter) in cases {
        let ast = Parser::new(Lexer::new(source).lex().unwrap())
            .parse_program()
            .unwrap();
        Checker::new().check(&ast).unwrap();
        let native = ir::task_lower::lower_program(&ast).unwrap();
        assert_eq!(native.tasks.functions.len(), 1);
        assert_eq!(native.entry, TaskFunctionId(0));
        let task = &native.tasks.functions[0];
        assert!(task
            .states
            .iter()
            .any(|state| matches!(state.terminator, TaskTerminator::Suspend { .. })));
        assert!(task.states.iter().all(|state| {
            !matches!(state.terminator, TaskTerminator::Await { .. })
                && state
                    .operations
                    .iter()
                    .all(|op| !matches!(op, TaskOp::Start { .. }))
        }));
        let plan = verify_and_plan(&native.tasks, TaskLimits::default()).unwrap();
        let counter_check = if has_counter {
            let index = task.states[task.entry.0]
                .operations
                .iter()
                .find_map(|op| match op {
                    TaskOp::Copy { dst, .. } => Some(*dst),
                    _ => None,
                })
                .expect("source counter binding");
            assert!(plan.functions[0].slots.contains(&index));
            // A body scope drain and its Suspend latch have both returned.
            format!("CHECK(fixture_parent->frame.s_{} == 1);", index.0)
        } else {
            String::new()
        };
        let mut generated = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
        let instrumentation = format!("{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{}\n{}\n{}\nstatic void fixture_source_resume(void*);\nstatic void fixture_source_poll_returned(void*,uint32_t);\n",
            native_task_ledger::LEDGER_LOCK, native_allocation_harness::ALLOCATION_HOOK,
            native_task_ledger::LOCKED_ALLOCATIONS);
        generated = replace_once(
            generated,
            "typedef struct KuString {",
            &format!("{instrumentation}typedef struct KuString {{"),
        );
        let resume = "static uint32_t ku_task_0_resume(void* raw) {";
        generated = replace_once(
            generated,
            resume,
            &format!("{resume}\n  fixture_source_resume(raw);"),
        );
        // This is AFTER the real control poll, scheduler state publication and
        // lease release, with no driver mutex held. It adds no fake progress.
        let returned = "    if (registry.control) ku_task_control_lease_release(&registry);\n    if (ku_task_driver_lock(driver)) return;";
        generated = replace_once(
            generated,
            returned,
            "    if (registry.control) ku_task_control_lease_release(&registry);\n    fixture_source_poll_returned(driver,outcome);\n    if (ku_task_driver_lock(driver)) return;",
        );
        generated = replace_once(
            generated,
            "int main(void) {",
            "static int fixture_original_main(void) {",
        );
        assert!(!generated.contains("run_source") && !generated.contains("const SOURCE"));
        generated.push_str(&SOURCE_WHILE_MAIN.replace("@COUNTER_CHECK@", &counter_check));
        run_generated(
            generated,
            label,
            "source-while returned_polls_before_cancel=2 ledger=0\n",
        );
    }
}

const SOURCE_WHILE_MAIN: &str = r#"
static KuTestEvent fixture_two_polls,fixture_release;
static unsigned fixture_resumes,fixture_returned_polls;
static KuTaskInstance_0* fixture_parent;
static void fixture_source_resume(void* raw) {
  if (!fixture_parent) fixture_parent=(KuTaskInstance_0*)raw;
  CHECK(fixture_parent==raw); fixture_resumes++;
}
static void fixture_source_poll_returned(void* raw,uint32_t outcome) {
  KuTaskDriverV1* driver=(KuTaskDriverV1*)raw;
  fixture_returned_polls++;
  if (fixture_returned_polls==2u) {
    CHECK(outcome==KU_TASK_CONTROL_PENDING && fixture_resumes==2u);
    KuTaskDriverSnapshotV1 snapshot={0};
    CHECK(ku_task_driver_snapshot(driver,&snapshot)==KU_TASK_DRIVER_OK);
    CHECK(snapshot.polls==2u && snapshot.queued==1u && !snapshot.running);
    CHECK(ku_test_event_set(&fixture_two_polls));
    CHECK(ku_test_event_wait(&fixture_release,2000u));
  }
}
static KuTaskDriverSnapshotV1 fixture_source_idle(KuTaskDriverV1* driver,uint64_t deadline) {
  CHECK(ku_task_driver_wait_idle(driver,deadline)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot={0}; CHECK(ku_task_driver_snapshot(driver,&snapshot)==KU_TASK_DRIVER_OK);
  CHECK(!snapshot.fault && !snapshot.clock_fault && !snapshot.running && !snapshot.queued && !snapshot.building);
  CHECK(snapshot.worker_waiting || snapshot.worker_exited); return snapshot;
}
int main(void) {
  CHECK(KU_TASK_FRAME_ABI_VERSION==4u && KU_TASK_CONTROL_ABI_VERSION==2u && KU_TASK_DRIVER_ABI_VERSION==6u);
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes);
  CHECK(ku_test_event_init(&fixture_two_polls) && ku_test_event_init(&fixture_release));
  KuTaskDriverV1* driver=(KuTaskDriverV1*)calloc(1,sizeof(*driver));
  KuTaskDriverSlotV1* slots=(KuTaskDriverSlotV1*)calloc(1,sizeof(*slots));
  size_t* ring=(size_t*)calloc(1,sizeof(*ring)); CHECK(driver && slots && ring);
  size_t fixed=sizeof(*driver)+sizeof(*slots)+sizeof(*ring);
  CHECK(ku_task_driver_init(driver,sizeof(*driver),KU_TASK_DRIVER_ABI_VERSION,slots,1,ring,1,
      fixed+sizeof(KuTaskInstance_0))==KU_TASK_DRIVER_OK);
  KuTaskValueV1 root={0}; CHECK(ku_task_0_start_value(driver,&root)==KU_TASK_DRIVER_OK && root.tag==KU_TASK_VALUE_LIVE);
  /* Two-second event bounds only detect missing startup/progress. They are not
   * cleanup budgets. The hook observes completed polls; it cannot split one. */
  CHECK(ku_test_event_wait(&fixture_two_polls,2000u));
  CHECK(fixture_parent==(KuTaskInstance_0*)root.owner.lease.control);
  CHECK(fixture_resumes==2u && fixture_returned_polls==2u);
  KuTaskDriverSnapshotV1 blocked={0}; CHECK(ku_task_driver_snapshot(driver,&blocked)==KU_TASK_DRIVER_OK);
  CHECK(blocked.polls==2u && blocked.queued==1u && !blocked.running && !blocked.building
      && blocked.resident==1u && blocked.reserved_bytes==sizeof(KuTaskInstance_0)
      && !blocked.fault && !blocked.clock_fault);
  CHECK(ku_task_control_atomic_load(&fixture_parent->control.phase)==KU_TASK_CONTROL_LIVE
      && !ku_task_control_atomic_load(&fixture_parent->control.executor));
  CHECK(fixture_parent->frame.header.status==KU_TASK_FRAME_PENDING);
  @COUNTER_CHECK@
  CHECK(fixture_ledger().allocations==4u && fixture_ledger().bytes==fixed+sizeof(KuTaskInstance_0));
  uint64_t now=ku_task_driver_now_ms(); CHECK(now!=UINT64_MAX && now<=UINT64_MAX-1000u);
  uint64_t deadline=now+1000u;
  CHECK(ku_task_driver_request_cancel(&root.ticket,&root.owner.lease,KU_TASK_CONTROL_CANCELLED,deadline)==KU_TASK_CONTROL_OK);
  CHECK(ku_test_event_set(&fixture_release));
  KuTaskDriverSnapshotV1 terminal=fixture_source_idle(driver,deadline);
  CHECK(terminal.resident==1u && terminal.terminal_held==1u);
  CHECK(ku_task_control_atomic_load(&fixture_parent->control.phase)==KU_TASK_CONTROL_CANCELLED
      && ku_task_control_cleanup_deadline(&fixture_parent->control)==deadline);
  CHECK(fixture_parent->control.frame_destroyed && !fixture_parent->frame_initialized
      && !fixture_parent->payload_initialized && !fixture_parent->frame.header.initialized
      && fixture_parent->frame.header.status==KU_TASK_FRAME_DESTROYED);
  KuTaskAdapterOutcomeV1 outcome={0};
  CHECK(ku_task_value_take(&root,NULL,&outcome)==KU_TASK_CONTROL_CANCELLED && ku_task_outcome_empty(&outcome,3u));
  CHECK(ku_task_value_drop(&root,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_shutdown(driver,deadline)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 empty=fixture_source_idle(driver,deadline);
  CHECK(!empty.resident && !empty.reserved_bytes && !empty.retiring && !empty.terminal_held);
#if defined(_WIN32)
  now=ku_task_driver_now_ms(); CHECK(now!=UINT64_MAX);
  DWORD remaining=now<deadline ? (DWORD)(deadline-now) : 0u;
  CHECK(WaitForSingleObject(driver->thread,remaining)==WAIT_OBJECT_0);
#endif
  CHECK(ku_task_driver_destroy(driver)==KU_TASK_DRIVER_OK);
  now=ku_task_driver_now_ms(); CHECK(now!=UINT64_MAX && now<=deadline);
  /* The worker was joined above. Post-poll observations occur outside the
   * driver mutex, so final ordinary counters are read only after that join. */
  CHECK(fixture_resumes==2u && fixture_returned_polls>=3u && empty.polls==fixture_returned_polls);
  free(ring); free(slots); free(driver);
  CHECK(ku_test_event_destroy(&fixture_two_polls) && ku_test_event_destroy(&fixture_release));
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes && !fixture_ledger().overflow);
  now=ku_task_driver_now_ms(); CHECK(now!=UINT64_MAX && now<=deadline);
  puts("source-while returned_polls_before_cancel=2 ledger=0");
  return 0;
}
"#;

const C_MAIN: &str = r#"
static KuTestEvent fixture_second_start,fixture_release;
static unsigned fixture_resumes,fixture_starts,fixture_results;
static KuTaskInstance_0* fixture_parent;
static void fixture_resume(void* raw) {
  if (!fixture_parent) fixture_parent=(KuTaskInstance_0*)raw;
  CHECK(fixture_parent==raw && fixture_resumes<2u); fixture_resumes++;
}
static int fixture_text(KuString value,const char* expected) {
  size_t length=strlen(expected); return value.len==length && !memcmp(value.ptr,expected,length);
}
static void fixture_inline(void* raw,uint32_t status) {
  KuTaskValueV1* value=(KuTaskValueV1*)raw;
  CHECK(status==KU_TASK_DRIVER_LIMIT && value->tag==KU_TASK_VALUE_INLINE_FAILED
      && value->result_kind==1u && value->rejection_code==KU_TASK_DRIVER_LIMIT
      && !value->owner.lease.control && !value->ticket.driver);
  fixture_starts++; CHECK(fixture_starts<=2u);
  if (fixture_starts==2u) {
    CHECK(fixture_results==1u);
    /* Observe the second actual admission rejection. No phase/return write. */
    CHECK(ku_test_event_set(&fixture_second_start));
    CHECK(ku_test_event_wait(&fixture_release,2000u));
  }
}
static void fixture_awaited(void* raw) {
  KuTaskAdapterOutcomeV1* outcome=(KuTaskAdapterOutcomeV1*)raw;
  CHECK(!fixture_results++ && outcome->result_kind==1u && outcome->exit_class==KU_TASK_EXIT_USER_RESULT
      && !outcome->value.integer.ok && !outcome->has_cleanup_deadline);
  CHECK(fixture_text(outcome->value.integer.error.domain,"task")
      && fixture_text(outcome->value.integer.error.code,"too_many_tasks"));
}
static KuTaskDriverSnapshotV1 fixture_idle(KuTaskDriverV1* driver,uint64_t deadline) {
  CHECK(ku_task_driver_wait_idle(driver,deadline)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot={0}; CHECK(ku_task_driver_snapshot(driver,&snapshot)==KU_TASK_DRIVER_OK);
  CHECK(!snapshot.fault && !snapshot.clock_fault && !snapshot.running && !snapshot.queued && !snapshot.building);
  CHECK(snapshot.worker_waiting || snapshot.worker_exited); return snapshot;
}
int main(void) {
  CHECK(KU_TASK_FRAME_ABI_VERSION==4u && KU_TASK_CONTROL_ABI_VERSION==2u && KU_TASK_DRIVER_ABI_VERSION==6u);
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes);
  CHECK(ku_test_event_init(&fixture_second_start) && ku_test_event_init(&fixture_release));
  KuTaskDriverV1* driver=(KuTaskDriverV1*)calloc(1,sizeof(*driver));
  KuTaskDriverSlotV1* slots=(KuTaskDriverSlotV1*)calloc(1,sizeof(*slots));
  size_t* ring=(size_t*)calloc(1,sizeof(*ring)); CHECK(driver && slots && ring);
  size_t fixed=sizeof(*driver)+sizeof(*slots)+sizeof(*ring);
  CHECK(ku_task_driver_init(driver,sizeof(*driver),KU_TASK_DRIVER_ABI_VERSION,slots,1,ring,1,
      fixed+sizeof(KuTaskInstance_0)+sizeof(KuTaskInstance_1))==KU_TASK_DRIVER_OK);
  KuTaskValueV1 root={0}; CHECK(ku_task_0_start_value(driver,&root)==KU_TASK_DRIVER_OK && root.tag==KU_TASK_VALUE_LIVE);
  CHECK(ku_test_event_wait(&fixture_second_start,2000u));
  /* The callback is held by a real event, so these ordinary reads have an HB
   * edge and cannot race further dispatch. polls counts returned R2 polls. */
  CHECK(fixture_parent==(KuTaskInstance_0*)root.owner.lease.control && fixture_starts==2u && fixture_results==1u);
  unsigned observed_resumes=fixture_resumes;
  KuTaskDriverSnapshotV1 blocked={0}; CHECK(ku_task_driver_snapshot(driver,&blocked)==KU_TASK_DRIVER_OK);
  CHECK(blocked.running==1u && !blocked.queued && !blocked.building && blocked.resident==1u
      && blocked.reserved_bytes==sizeof(KuTaskInstance_0) && !blocked.fault && !blocked.clock_fault);
  CHECK(ku_task_control_atomic_load(&fixture_parent->control.phase)==KU_TASK_CONTROL_LIVE);
  CHECK(fixture_ledger().allocations==4u && fixture_ledger().bytes==fixed+sizeof(KuTaskInstance_0));
  uint64_t now=ku_task_driver_now_ms();
  uint64_t cleanup_deadline=now+1000u,deadline=now+2000u;
  CHECK(ku_task_driver_request_cancel(&root.ticket,&root.owner.lease,KU_TASK_CONTROL_CANCELLED,cleanup_deadline)==KU_TASK_CONTROL_OK);
  CHECK(ku_test_event_set(&fixture_release)); fixture_idle(driver,deadline);
  CHECK(ku_task_control_atomic_load(&fixture_parent->control.phase)==KU_TASK_CONTROL_CANCELLED
      && ku_task_control_cleanup_deadline(&fixture_parent->control)==cleanup_deadline);
  CHECK(fixture_starts==2u && fixture_results==1u && fixture_resumes==observed_resumes);
  CHECK(fixture_parent->control.frame_destroyed && !fixture_parent->frame_initialized
      && !fixture_parent->payload_initialized && !fixture_parent->frame.header.initialized
      && fixture_parent->frame.header.status==KU_TASK_FRAME_DESTROYED);
  KuTaskAdapterOutcomeV1 outcome={0};
  CHECK(ku_task_value_take(&root,NULL,&outcome)==KU_TASK_CONTROL_CANCELLED && ku_task_outcome_empty(&outcome,3u));
  CHECK(ku_task_value_drop(&root,cleanup_deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_shutdown(driver,cleanup_deadline)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 empty=fixture_idle(driver,deadline);
  CHECK(!empty.resident && !empty.reserved_bytes);
#if defined(_WIN32)
  CHECK(WaitForSingleObject(driver->thread,2000u)==WAIT_OBJECT_0);
#else
  alarm(2);
#endif
  CHECK(ku_task_driver_destroy(driver)==KU_TASK_DRIVER_OK);
#if !defined(_WIN32)
  alarm(0);
#endif
  free(ring); free(slots); free(driver);
  CHECK(ku_test_event_destroy(&fixture_second_start) && ku_test_event_destroy(&fixture_release));
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes && !fixture_ledger().overflow);
  printf("await-progress resumes=%u returned_polls=%llu ledger=0\n",observed_resumes,(unsigned long long)blocked.polls);
  if (observed_resumes!=2u || blocked.polls!=1u) {
    fputs("Await-only cycle reentered Start without returning its first poll\n",stderr); return 1;
  }
  return 0;
}
"#;
