//! Repeated real Await Pending must not replay a left snapshot or child Start.
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
    ir::{
        task::{self, TaskBinaryOp, TaskOp, TaskTerminator},
        task_lower,
    },
    lexer::Lexer,
    parser::Parser,
};
use native_allocation_harness::ALLOCATION_HOOK;
use native_harness::{
    compile_harness, run_bounded, TempDir, NATIVE_THREAD_LIFECYCLE_HARNESS, RUN_LIMITS, RUN_TIMEOUT,
};
use native_task_ledger::{LEDGER_LOCK, LOCKED_ALLOCATIONS};
use std::{fs, process::Command};

fn replace_once(source: String, anchor: &str, replacement: &str) -> String {
    assert_eq!(source.matches(anchor).count(), 1, "pending hook: {anchor}");
    source.replacen(anchor, replacement, 1)
}

#[test]
fn native_task_expression_repeated_pending_preserves_lhs_and_starts_rhs_once() {
    let source = r#"
async fn Parent(base: int): int! { return ok(base + (await Right())?) }
async fn Right(): int! { return ok(5) }
async fn main(): null! { return ok(null) }
"#;
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let native = task_lower::lower_program(&ast).unwrap();
    assert_eq!(native.tasks.functions[0].name, "Parent");
    assert_eq!(native.tasks.functions[1].name, "Right");
    let parent = &native.tasks.functions[0];
    let lhs: Vec<_> = parent
        .states
        .iter()
        .flat_map(|state| &state.operations)
        .filter_map(|op| match op {
            TaskOp::Binary {
                op: TaskBinaryOp::Add,
                left,
                ..
            } => Some(*left),
            _ => None,
        })
        .collect();
    assert_eq!(lhs.len(), 1);
    let lhs = lhs[0];
    let base = parent.parameters[0];
    assert_ne!(lhs, base);
    assert_eq!(
        parent
            .states
            .iter()
            .flat_map(|state| &state.operations)
            .filter(|op| { matches!(op, TaskOp::Copy { dst, src } if *dst == lhs && *src == base) })
            .count(),
        1
    );
    let awaits: Vec<_> = parent
        .states
        .iter()
        .enumerate()
        .filter_map(|(state, body)| match &body.terminator {
            TaskTerminator::Await { task, .. } => Some((state, *task)),
            _ => None,
        })
        .collect();
    assert_eq!(awaits.len(), 1);
    let (await_state, child_slot) = awaits[0];
    assert_ne!(await_state, parent.entry.0);
    let plan = task::verify_and_plan(&native.tasks, Default::default()).unwrap();
    assert!(plan.functions[0].slots.contains(&lhs));
    assert!(plan.functions[0].slots.contains(&child_slot));
    let generated = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
    let instrumentation = format!(
        "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\n{LOCKED_ALLOCATIONS}\nstatic void fixture_lhs(void);\nstatic void fixture_start(void);\nstatic void fixture_drive(uint32_t);\nstatic uint32_t fixture_hold_right(void*);\n"
    );
    let generated = replace_once(
        generated,
        "typedef struct KuString {",
        &format!("{instrumentation}typedef struct KuString {{"),
    );
    let lhs_assignment = format!("  frame->s_{} = frame->s_{};\n", lhs.0, base.0);
    let generated = replace_once(
        generated,
        &lhs_assignment,
        &format!("  fixture_lhs();\n{lhs_assignment}"),
    );
    let generated = replace_once(
        generated,
        "static uint32_t ku_task_frame_0_drive(KuTaskFrame_0* frame, const KuTaskFrameClockV1* clock, int cleanup) {",
        "static uint32_t ku_task_frame_0_drive(KuTaskFrame_0* frame, const KuTaskFrameClockV1* clock, int cleanup) {\n  CHECK(!cleanup); fixture_drive(frame->header.state);",
    );
    let generated = replace_once(
        generated,
        "static uint32_t ku_task_1_try_start_impl(KuTaskDriverV1* driver,\n    const KuTaskDriverStartChargeV1* source, KuTaskHandle_1* output) {",
        "static uint32_t ku_task_1_try_start_impl(KuTaskDriverV1* driver,\n    const KuTaskDriverStartChargeV1* source, KuTaskHandle_1* output) {\n  fixture_start();",
    );
    let generated = replace_once(
        generated,
        "static uint32_t ku_task_1_resume(void* raw) {",
        "static uint32_t ku_task_1_resume(void* raw) {\n  uint32_t held=fixture_hold_right(raw);\n  if (held!=UINT32_MAX) return held;",
    );
    let mut generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_unmodified_source_main(void) {",
    );
    generated.push_str(
        &C_MAIN
            .replace("@LHS@", &lhs.0.to_string())
            .replace("@CHILD@", &child_slot.0.to_string())
            .replace("@ENTRY@", &parent.entry.0.to_string())
            .replace("@AWAIT@", &await_state.to_string()),
    );
    let directory = TempDir::new("native-task-expression-pending");
    let path = directory.path().join("program.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute the real repeated-Pending expression path"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("repeated Pending fixture ends under the real process watchdog");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-expression-pending-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const C_MAIN: &str = r#"
static KuAtomicRefcount fixture_hold;
static KuTestEvent fixture_right_parked;
static unsigned fixture_lhs_count,fixture_start_count,fixture_drive_count,fixture_right_count;
static void fixture_lhs(void) { CHECK(!fixture_lhs_count++); }
static void fixture_start(void) { CHECK(!fixture_start_count++); }
static void fixture_drive(uint32_t state) {
  CHECK(state==(fixture_drive_count ? @AWAIT@u : @ENTRY@u));
  CHECK(fixture_drive_count<5u); fixture_drive_count++;
}
static uint32_t fixture_hold_right(void* raw) {
  KuTaskInstance_1* instance=(KuTaskInstance_1*)raw;
  fixture_right_count++;
  if (ku_task_control_atomic_load(&fixture_hold)) {
    CHECK(fixture_right_count==1u);
    /* Pause only Right's progress. The real driver parks a LIVE R2 control;
     * no fixture evaluates its body, produces a result, or completes Await. */
    CHECK(ku_task_driver_set_intent(&instance->ticket,KU_TASK_DRIVER_WAIT)==KU_TASK_DRIVER_OK);
    CHECK(ku_test_event_set(&fixture_right_parked));
    return KU_TASK_CONTROL_PENDING;
  }
  CHECK(fixture_right_count==2u);
  return UINT32_MAX;
}
static KuTaskDriverSnapshotV1 fixture_idle(KuTaskDriverV1* driver,uint64_t deadline) {
  CHECK(ku_task_driver_wait_idle(driver,deadline)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot={0};
  CHECK(ku_task_driver_snapshot(driver,&snapshot)==KU_TASK_DRIVER_OK);
  CHECK(!snapshot.fault && !snapshot.clock_fault && !snapshot.queued && !snapshot.running && !snapshot.building);
  CHECK(snapshot.worker_waiting || snapshot.worker_exited);
  return snapshot;
}
static void fixture_wait(KuTaskInstance_0* parent,const KuTaskDriverWaitTokenV1* original,
    const KuTaskDriverTicketV1* child) {
  CHECK(parent->wait.driver==original->driver && parent->wait.parent_slot==original->parent_slot
      && parent->wait.parent_generation==original->parent_generation && parent->wait.epoch==original->epoch);
  KuTaskDriverWaitSnapshotV1 wait={0};
  CHECK(ku_task_driver_wait_read(&parent->wait,&wait)==KU_TASK_DRIVER_OK);
  CHECK(wait.kind==KU_TASK_DRIVER_WAIT_KIND_RESULT && wait.state==KU_TASK_DRIVER_WAIT_ARMED);
  CHECK(wait.child_slot==child->slot && wait.child_generation==child->generation);
  CHECK(parent->frame.header.state==@AWAIT@u && parent->frame.header.status==KU_TASK_FRAME_PENDING);
  CHECK(parent->frame.header.initialized&(UINT64_C(1)<<@LHS@));
  CHECK(parent->frame.s_@LHS@==37);
  CHECK(parent->frame.header.initialized&(UINT64_C(1)<<@CHILD@));
  CHECK(parent->frame.s_@CHILD@.tag==KU_TASK_VALUE_LIVE && parent->frame.s_@CHILD@.ticket.driver==child->driver
      && parent->frame.s_@CHILD@.ticket.slot==child->slot && parent->frame.s_@CHILD@.ticket.generation==child->generation);
  CHECK(!parent->payload_initialized && !parent->frame.header.result_initialized);
}
int main(void) {
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes);
  ku_task_control_atomic_init(&fixture_hold,1);
  CHECK(ku_test_event_init(&fixture_right_parked));
  KuTaskDriverV1* driver=(KuTaskDriverV1*)calloc(1,sizeof(*driver));
  KuTaskDriverSlotV1* slots=(KuTaskDriverSlotV1*)calloc(2,sizeof(*slots));
  size_t* ring=(size_t*)calloc(2,sizeof(*ring)); CHECK(driver && slots && ring);
  size_t fixed=sizeof(*driver)+2u*(sizeof(*slots)+sizeof(*ring));
  size_t instances=sizeof(KuTaskInstance_0)+sizeof(KuTaskInstance_1);
  CHECK(ku_task_driver_init(driver,sizeof(*driver),KU_TASK_DRIVER_ABI_VERSION,
      slots,2,ring,2,fixed+instances)==KU_TASK_DRIVER_OK);
  int64_t base=37; KuTaskValueV1 root={0};
  uint64_t deadline=ku_task_driver_now_ms()+2000u;
  CHECK(ku_task_0_start_value(driver,&base,&root)==KU_TASK_DRIVER_OK && base==37);
  CHECK(ku_test_event_wait(&fixture_right_parked,2000));
  KuTaskDriverSnapshotV1 parked=fixture_idle(driver,deadline);
  CHECK(parked.resident==2u && parked.reserved_bytes==instances);
  KuTaskInstance_0* parent=(KuTaskInstance_0*)root.owner.lease.control;
  CHECK(fixture_lhs_count==1u && fixture_start_count==1u && fixture_drive_count==1u && fixture_right_count==1u);
  KuTaskDriverWaitTokenV1 wait=parent->wait; CHECK(wait.driver==driver && wait.epoch);
  KuTaskDriverTicketV1 child=parent->frame.s_@CHILD@.ticket;
  KuTaskControlLeaseV1 observer={0};
  CHECK(!ku_task_driver_lock(driver));
  KuTaskDriverSlotV1* child_slot=ku_task_driver_find(&child);
  CHECK(child_slot && child_slot->state==KU_TASK_DRIVER_PARKED && child_slot->binding);
  CHECK(child_slot->binding->operations.resume==ku_task_1_resume);
  CHECK(ku_task_control_lease_retain(&child_slot->driver_lease,&observer)==KU_TASK_CONTROL_OK);
  CHECK(!ku_task_driver_unlock(driver));
  fixture_wait(parent,&wait,&child);
  FixtureLedger before=fixture_ledger(); CHECK(before.allocations==5u && before.bytes==fixed+instances);
  uint64_t polls=parked.polls;
  /* Exactly three independent notifications, each followed by the real
   * worker's condition-wait acknowledgement; no sleep or busy polling. */
  for (size_t i=0;i<3;i++) {
    CHECK(ku_task_driver_wake(&root.ticket)==KU_TASK_DRIVER_OK);
    KuTaskDriverSnapshotV1 again=fixture_idle(driver,deadline);
    CHECK(again.polls==polls+1u && again.resident==2u && again.reserved_bytes==instances); polls=again.polls;
    CHECK(fixture_drive_count==i+2u && fixture_lhs_count==1u && fixture_start_count==1u && fixture_right_count==1u);
    CHECK(ku_task_control_atomic_load(&observer.control->phase)==KU_TASK_CONTROL_LIVE);
    fixture_wait(parent,&wait,&child);
    FixtureLedger unchanged=fixture_ledger();
    CHECK(unchanged.allocations==before.allocations && unchanged.bytes==before.bytes && unchanged.calls==before.calls);
  }
  /* Only now may the real Right frame produce 5. Real Await takes its Result,
   * the generated checked Add consumes the preserved 37, and the host drops
   * Right's actual source owner. The observation lease protects later reads. */
  ku_task_control_atomic_store(&fixture_hold,0);
  CHECK(ku_task_driver_wake(&child)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_wait_result(&root.ticket,&root.owner.lease,deadline)==KU_TASK_DRIVER_WAIT_READY);
  KuTaskDriverSnapshotV1 ready=fixture_idle(driver,deadline);
  CHECK(ready.resident==2u && ready.reserved_bytes==instances);
  CHECK(fixture_drive_count==5u && fixture_lhs_count==1u && fixture_start_count==1u && fixture_right_count==2u);
  CHECK(!parent->wait.driver && !parent->frame.header.initialized && parent->control.frame_destroyed);
  CHECK(ku_task_control_atomic_load(&observer.control->phase)==KU_TASK_CONTROL_COMPLETED);
  CHECK(ku_task_control_atomic_load(&observer.control->payload)==KU_TASK_CONTROL_PAYLOAD_TAKEN);
  CHECK(observer.control->frame_destroyed && !ku_task_control_atomic_load(&observer.control->lifecycle_pin));
  CHECK(fixture_ledger().calls==before.calls);
  KuTaskAdapterOutcomeV1 outcome={0};
  CHECK(ku_task_value_take(&root,NULL,&outcome)==KU_TASK_CONTROL_OK);
  CHECK(outcome.result_kind==1u && outcome.exit_class==KU_TASK_EXIT_USER_RESULT && outcome.value.integer.ok && outcome.value.integer.value==42);
  CHECK(!outcome.has_cleanup_deadline && !outcome.cleanup_deadline);
  ku_task_outcome_drop(&outcome);
  CHECK(ku_task_value_drop(&root,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_control_lease_release(&observer)==KU_TASK_CONTROL_OK);
  CHECK(ku_task_driver_shutdown(driver,deadline)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 empty=fixture_idle(driver,deadline);
  CHECK(!empty.resident && !empty.reserved_bytes);
#if defined(_WIN32)
  CHECK(WaitForSingleObject(driver->thread,2000)==WAIT_OBJECT_0);
#else
  alarm(2);
#endif
  CHECK(ku_task_driver_destroy(driver)==KU_TASK_DRIVER_OK);
#if !defined(_WIN32)
  alarm(0);
#endif
  free(ring); free(slots); free(driver);
  CHECK(ku_test_event_destroy(&fixture_right_parked));
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes && !fixture_ledger().overflow);
  puts("task-expression-pending-ok"); return 0;
}
"#;
