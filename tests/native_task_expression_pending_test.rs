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

#[test]
fn native_task_source_if_owned_inputs_await_pending_and_normal_or_cancelled_scope_in_c() {
    let source = r#"
async fn Parent(gate: bool, local_input: str, child_input: str, returned: str): str! {
    if (gate) {
        held = local_input
        pending = Child(child_input)
        number = (await Ready(7))?
        println(number)
    }
    println("joined")
    return ok(returned)
}
async fn Child(value: str): str! { return ok(value) }
async fn Ready(value: int): int! { return ok(value) }
async fn main(): null! { return ok(null) }
"#;
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let native = task_lower::lower_program(&ast).unwrap();
    assert_eq!(
        native
            .tasks
            .functions
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>(),
        vec!["Parent", "Child", "Ready", "main"]
    );
    let parent = &native.tasks.functions[0];
    assert_eq!(parent.parameters.len(), 4);
    let operations: Vec<_> = parent
        .states
        .iter()
        .flat_map(|state| &state.operations)
        .collect();
    let held: Vec<_> = operations
        .iter()
        .filter_map(|op| match op {
            TaskOp::Move { dst, src } if *src == parent.parameters[1] => Some(*dst),
            _ => None,
        })
        .collect();
    assert_eq!(held.len(), 1);
    let child_start: Vec<_> = operations
        .iter()
        .filter_map(|op| match op {
            TaskOp::Start { dst, function, .. } if function.0 == 1 => Some(*dst),
            _ => None,
        })
        .collect();
    assert_eq!(child_start.len(), 1);
    assert_eq!(
        operations
            .iter()
            .filter(|op| matches!(op, TaskOp::Start { function, .. } if function.0 == 2))
            .count(),
        1
    );
    let child: Vec<_> = operations
        .iter()
        .filter_map(|op| match op {
            TaskOp::Move { dst, src } if *src == child_start[0] => Some(*dst),
            _ => None,
        })
        .collect();
    assert_eq!(child.len(), 1);
    let awaits: Vec<_> = parent
        .states
        .iter()
        .enumerate()
        .filter_map(|(id, state)| match state.terminator {
            TaskTerminator::Await { task, .. } => Some((id, task)),
            _ => None,
        })
        .collect();
    let drains: Vec<_> = parent
        .states
        .iter()
        .enumerate()
        .filter_map(|(id, state)| match state.terminator {
            TaskTerminator::ScopeDrain { ready, cleanup, .. } => Some((id, ready, cleanup)),
            _ => None,
        })
        .collect();
    let joined: Vec<_> = operations
        .iter()
        .filter_map(|op| match op {
            TaskOp::Init {
                dst,
                value: task::TaskConstant::Str(text),
            } if text == "joined" => Some(*dst),
            _ => None,
        })
        .collect();
    assert_eq!(awaits.len(), 1);
    assert_eq!(drains.len(), 1);
    assert_eq!(joined.len(), 1);
    let plan = task::verify_and_plan(&native.tasks, Default::default()).unwrap();
    assert_eq!(plan.functions[0].scopes.len(), 1);
    assert!(plan.functions[0].slots.contains(&held[0]));
    let task_slots: Vec<_> = parent
        .slots
        .iter()
        .enumerate()
        .filter(|(_, slot)| matches!(slot.ty, task::TaskSlotType::Task { .. }))
        .map(|(id, _)| task::SlotId(id))
        .collect();
    let receipt = task_slots
        .iter()
        .position(|slot| *slot == child[0])
        .unwrap();
    assert_ne!(
        plan.functions[0].scopes[0].task_mask & (1u64 << child[0].0),
        0
    );
    let mut generated = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
    for required in [
        "#define KU_TASK_FRAME_ABI_VERSION 4u",
        "#define KU_TASK_CONTROL_ABI_VERSION 2u",
        "#define KU_TASK_DRIVER_ABI_VERSION 6u",
    ] {
        assert!(generated.contains(required));
    }
    assert!(!generated.contains("run_source") && !generated.contains("const SOURCE"));
    let ledger = replace_once(
        LOCKED_ALLOCATIONS.to_owned(),
        "ku_perf_free(p);",
        "fixture_if_free(p); ku_perf_free(p);",
    );
    let instrumentation = format!(
        "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\nstatic void fixture_if_free(void*);\n{ledger}\nstatic void fixture_if_start(unsigned);\nstatic int fixture_if_pause(void*);\nstatic void fixture_if_drive(void*,int);\nstatic uint32_t fixture_if_child_hold(void*,int);\nstatic uint32_t fixture_if_ready_hold(void*);\nstatic void fixture_if_continued(void*,uint64_t);\nstatic void fixture_if_joined(void);\n"
    );
    generated = replace_once(
        generated,
        "typedef struct KuString {",
        &format!("{instrumentation}typedef struct KuString {{"),
    );
    for (id, declaration) in [
        (1, "static uint32_t ku_task_1_try_start_impl(KuTaskDriverV1* driver,\n    const KuTaskDriverStartChargeV1* source, KuString* arg_0, KuTaskHandle_1* output) {"),
        (2, "static uint32_t ku_task_2_try_start_impl(KuTaskDriverV1* driver,\n    const KuTaskDriverStartChargeV1* source, int64_t* arg_0, KuTaskHandle_2* output) {"),
    ] {
        generated = replace_once(generated, declaration, &format!("{declaration}\n  fixture_if_start({id}u);"));
    }
    let resume = "static uint32_t ku_task_0_resume(void* raw) {";
    generated = replace_once(
        generated,
        resume,
        &format!("{resume}\n  if (fixture_if_pause(raw)) return KU_TASK_CONTROL_PENDING;"),
    );
    let drive = "static uint32_t ku_task_frame_0_drive(KuTaskFrame_0* frame, const KuTaskFrameClockV1* clock, int cleanup) {";
    generated = replace_once(
        generated,
        drive,
        &format!("{drive}\n  fixture_if_drive(frame,cleanup);"),
    );
    for (entry, hook) in [
        ("static uint32_t ku_task_1_resume(void* raw) {", "fixture_if_child_hold(raw,0)"),
        ("static uint32_t ku_task_1_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {", "fixture_if_child_hold(raw,1)"),
        ("static uint32_t ku_task_2_resume(void* raw) {", "fixture_if_ready_hold(raw)"),
    ] {
        generated = replace_once(generated, entry, &format!("{entry}\n  uint32_t held={hook};\n  if (held!=UINT32_MAX) return held;"));
    }
    let continued = "status=ku_task_frame_0_scope_continue(&instance->frame,sizeof(instance->frame),KU_TASK_FRAME_ABI_VERSION,descriptor->scope_id);\n  if (status!=KU_TASK_FRAME_OK) return status;";
    generated = replace_once(
        generated,
        continued,
        &format!("{continued}\n  fixture_if_continued(instance,descriptor->scope_id);"),
    );
    let joined = joined[0].0;
    let joined_print = format!("  if ((frame->s_{joined}.len && fwrite(frame->s_{joined}.ptr,1,frame->s_{joined}.len,stdout)!=frame->s_{joined}.len) || fputc('\\n',stdout)==EOF || fflush(stdout)==EOF) {{");
    generated = replace_once(
        generated,
        &joined_print,
        &format!("  fixture_if_joined();\n{joined_print}"),
    );
    generated = replace_once(generated, "static uint32_t ku_task_driver_owner_drop_receipt(\n",
        "static uint32_t ku_task_driver_owner_drop_receipt(const KuTaskDriverTicketV1*,KuTaskControlOwnerV1*,uint64_t,KuTaskDriverCleanupReceiptV1*);\nstatic uint32_t fixture_if_real_owner_drop_receipt(\n");
    generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_if_unmodified_source_main(void) {",
    );
    let mut fixture = IF_OWNED_PENDING_MAIN.to_owned();
    for (marker, value) in [
        ("@HELD@", held[0].0),
        ("@CHILD@", child[0].0),
        ("@READY_TASK@", awaits[0].1 .0),
        ("@AWAIT@", awaits[0].0),
        ("@DRAIN@", drains[0].0),
        ("@SCOPE_READY@", drains[0].1 .0),
        ("@CLEANUP@", drains[0].2 .0),
        ("@ENTRY@", parent.entry.0),
        ("@RECEIPT@", receipt),
    ] {
        fixture = fixture.replace(marker, &value.to_string());
    }
    fixture = fixture.replace(
        "@TASK_MASK@",
        &plan.functions[0].scope_task_mask.to_string(),
    );
    generated.push_str(&fixture);
    let directory = TempDir::new("native-task-if-owned-pending");
    let path = directory.path().join("program.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute source If Owned/Pending"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("source If Pending remains under the existing real process watchdog");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "joined\n7\njoined\n7\nsource-if-owned-pending-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const IF_OWNED_PENDING_MAIN: &str = r#"
enum { IF_FALSE, IF_NORMAL, IF_CANCEL };
static unsigned if_mode,if_starts[3],if_entry,if_await_visits,if_ready_visits,if_cleanup_visits;
static unsigned if_closed,if_joined,if_transferred,if_ack_seen;
static unsigned if_child_resumes,if_ready_resumes,if_frees[3];
static uintptr_t if_ids[3];
static const size_t if_caps[3]={19u,23u,29u};
static KuAtomicRefcount if_release_ready,if_release_child,if_release_parent;
static KuTestEvent if_ready_parked,if_child_cleanup,if_scope_closed;
static KuTaskInstance_0* if_parent;
static KuTaskInstance_1* if_child;
static KuTaskInstance_2* if_ready;
static KuTaskControlLeaseV1 if_child_observer,if_ready_observer;
static KuTaskDriverCleanupReceiptV1 if_receipt;
static uint64_t if_scope_deadline;
static int if_empty_string(KuString s) { return !s.ptr && !s.len && !s.capacity && !s.storage; }
static int if_empty_result(KuResult_str r) {
  return !r.ok && if_empty_string(r.value) && if_empty_string(r.error.domain)
      && if_empty_string(r.error.code) && if_empty_string(r.error.message);
}
static void fixture_if_free(void* pointer) {
  if (!pointer) return;
  for (size_t id=0;id<3u;id++) if (if_ids[id] && if_ids[id]==(uintptr_t)pointer) {
    CHECK(!if_frees[id]++);
    if (if_mode==IF_FALSE) CHECK(if_joined==1u && !if_transferred && !if_closed);
    else {
      CHECK(if_transferred==1u);
      if (if_mode==IF_NORMAL && id==0u)
        CHECK(if_closed==1u && if_ack_seen && if_ready_visits==1u && !if_joined);
      if (if_mode==IF_NORMAL && id==2u) CHECK(if_joined==1u);
      if (if_mode==IF_CANCEL && id!=1u)
        CHECK(if_cleanup_visits==1u && !if_closed && !if_joined && !if_frees[1]);
    }
  }
}
static KuString if_owned(unsigned id,const char* text) {
  size_t length=strlen(text); CHECK(id<3u && length<if_caps[id]);
  uint8_t* pointer=(uint8_t*)malloc(if_caps[id]); CHECK(pointer);
  memcpy(pointer,text,length); if_ids[id]=(uintptr_t)pointer;
  KuString s={pointer,length,if_caps[id],KU_STRING_OWNED}; return s;
}
static void fixture_if_start(unsigned id) {
  CHECK(if_mode!=IF_FALSE && id>=1u && id<=2u && !if_starts[id]++);
}
static int fixture_if_pause(void* raw) {
  KuTaskInstance_0* parent=(KuTaskInstance_0*)raw;
  if (!if_parent) if_parent=parent;
  CHECK(parent==if_parent);
  if (if_mode==IF_NORMAL && if_closed && parent->frame.header.status==KU_TASK_FRAME_PENDING
      && parent->frame.header.state==@SCOPE_READY@u && !ku_task_control_atomic_load(&if_release_parent)) {
    CHECK(ku_task_driver_set_intent(&parent->ticket,KU_TASK_DRIVER_WAIT)==KU_TASK_DRIVER_OK);
    return 1;
  }
  return 0;
}
static void fixture_if_drive(void* raw,int cleanup) {
  KuTaskFrame_0* frame=(KuTaskFrame_0*)raw;
  if (cleanup) {
    CHECK(if_mode==IF_CANCEL && frame->header.state==@CLEANUP@u && !if_cleanup_visits++);
    CHECK(if_transferred==1u && !(frame->header.initialized&UINT64_C(@TASK_MASK@)));
  } else if (frame->header.state==@ENTRY@u) CHECK(!if_entry++);
  else if (frame->header.state==@AWAIT@u) { CHECK(if_mode!=IF_FALSE); if_await_visits++; }
  else {
    CHECK(if_mode==IF_NORMAL && frame->header.state==@SCOPE_READY@u && if_closed==1u && !if_ready_visits++);
    CHECK(!if_frees[0] && if_frees[1]==1u && !if_frees[2]);
  }
}
static void fixture_if_joined(void) {
  CHECK(if_mode!=IF_CANCEL && !if_joined++);
  if (if_mode==IF_NORMAL) CHECK(if_closed==1u && if_ready_visits==1u && if_frees[0]==1u && if_frees[1]==1u);
  else CHECK(!if_starts[1] && !if_starts[2] && !if_closed && !if_frees[0] && !if_frees[1]);
  CHECK(!if_frees[2]);
}
static void if_observe(KuTaskDriverTicketV1* ticket,KuTaskControlLeaseV1* observer) {
  CHECK(!ku_task_driver_lock(ticket->driver));
  KuTaskDriverSlotV1* slot=ku_task_driver_find(ticket);
  CHECK(slot && slot->binding && slot->driver_lease.control==slot->binding);
  CHECK(ku_task_control_lease_retain(&slot->driver_lease,observer)==KU_TASK_CONTROL_OK);
  CHECK(!ku_task_driver_unlock(ticket->driver));
}
static uint32_t fixture_if_child_hold(void* raw,int cleanup) {
  KuTaskInstance_1* child=(KuTaskInstance_1*)raw;
  if (!if_child) { if_child=child; if_observe(&child->ticket,&if_child_observer); }
  CHECK(if_mode!=IF_FALSE && if_child==child);
  if (!cleanup) {
    CHECK(!if_child_resumes++ && (uintptr_t)child->frame.s_0.ptr==if_ids[1]);
  } else {
    CHECK(if_transferred==1u && ku_task_control_atomic_load(&child->control.phase)==KU_TASK_CONTROL_REQUESTED_CANCEL);
    if (ku_task_control_atomic_load(&if_release_child)) return UINT32_MAX;
    CHECK(ku_test_event_set(&if_child_cleanup));
  }
  CHECK(ku_task_driver_set_intent(&child->ticket,KU_TASK_DRIVER_WAIT)==KU_TASK_DRIVER_OK);
  return KU_TASK_CONTROL_PENDING;
}
static uint32_t fixture_if_ready_hold(void* raw) {
  KuTaskInstance_2* ready=(KuTaskInstance_2*)raw;
  if (!if_ready) { if_ready=ready; if_observe(&ready->ticket,&if_ready_observer); }
  CHECK(if_mode!=IF_FALSE && ready==if_ready && ready->frame.s_0==7);
  if_ready_resumes++;
  if (ku_task_control_atomic_load(&if_release_ready)) { CHECK(if_ready_resumes==2u); return UINT32_MAX; }
  CHECK(if_ready_resumes==1u);
  CHECK(ku_task_driver_set_intent(&ready->ticket,KU_TASK_DRIVER_WAIT)==KU_TASK_DRIVER_OK);
  CHECK(ku_test_event_set(&if_ready_parked));
  return KU_TASK_CONTROL_PENDING;
}
static uint32_t ku_task_driver_owner_drop_receipt(const KuTaskDriverTicketV1* ticket,
    KuTaskControlOwnerV1* owner,uint64_t deadline,KuTaskDriverCleanupReceiptV1* receipt) {
  CHECK(if_mode!=IF_FALSE && if_parent && owner->lease.control==if_child_observer.control);
  CHECK(!if_transferred && !if_frees[0] && !if_frees[1] && !if_frees[2]);
  CHECK(if_parent->frame.header.status==KU_TASK_FRAME_SCOPE_REQUEST && if_parent->frame.header.state==@DRAIN@u);
  CHECK(if_parent->scope_mode==1u && if_parent->scope_expected_mask==(UINT64_C(1)<<@RECEIPT@));
  CHECK(receipt==&if_parent->receipts[@RECEIPT@] && deadline!=UINT64_MAX);
  uint32_t status=fixture_if_real_owner_drop_receipt(ticket,owner,deadline,receipt);
  CHECK(status==KU_TASK_DRIVER_OK && !owner->lease.control);
  if_receipt=*receipt; if_scope_deadline=deadline; if_transferred=1u;
  return status;
}
static void fixture_if_continued(void* raw,uint64_t scope) {
  KuTaskInstance_0* parent=(KuTaskInstance_0*)raw;
  CHECK(if_mode==IF_NORMAL && !scope && parent==if_parent && !if_closed++);
  CHECK(parent->frame.header.status==KU_TASK_FRAME_PENDING && parent->frame.header.state==@SCOPE_READY@u);
  CHECK(!if_ready_visits && !if_joined && !if_frees[0] && if_frees[1]==1u && !if_frees[2]);
  CHECK(ku_task_driver_cleanup_receipt_read(&if_receipt)==KU_TASK_DRIVER_CLEANUP_ACK);
  if_ack_seen=1u;
  CHECK(parent->scope_mode==1u && !parent->scope_token.driver && !parent->receipt_mask && !parent->wait.driver);
  CHECK(!ku_task_driver_lock(parent->ticket.driver));
  KuTaskDriverSlotV1* slot=ku_task_driver_find(&parent->ticket);
  CHECK(slot && !slot->scope_session_active && !slot->scope_wait_started && !slot->scope_receipts
      && slot->scope_wait_deadline==UINT64_MAX);
  CHECK(!ku_task_driver_unlock(parent->ticket.driver));
  CHECK(ku_test_event_set(&if_scope_closed));
}
static KuTaskDriverSnapshotV1 if_idle(KuTaskDriverV1* driver) {
  CHECK(ku_task_driver_wait_idle(driver,ku_task_driver_now_ms()+2000u)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot={0}; CHECK(ku_task_driver_snapshot(driver,&snapshot)==KU_TASK_DRIVER_OK);
  CHECK(!snapshot.fault && !snapshot.clock_fault && !snapshot.running && !snapshot.queued && !snapshot.building);
  CHECK(snapshot.worker_waiting || snapshot.worker_exited); return snapshot;
}
/* wait_idle supplies the worker-to-main happens-before edge, but a real
 * scope timer may subsequently wake it. Hold its queue mutex and check that
 * no callback is running before reading mutable instance/fixture fields. */
static void if_lock_idle(KuTaskDriverV1* driver) {
  CHECK(!ku_task_driver_lock(driver)); CHECK(!driver->running && !driver->queued);
}
static void if_unlock(KuTaskDriverV1* driver) { CHECK(!ku_task_driver_unlock(driver)); }
static void if_pending(KuTaskValueV1* root) {
  KuTaskAdapterOutcomeV1 pending={0};
  CHECK(ku_task_value_take(root,NULL,&pending)==KU_TASK_CONTROL_PENDING && ku_task_outcome_empty(&pending,4u));
  if_idle(root->ticket.driver);
}
static void if_wait_kind(KuTaskDriverWaitTokenV1* token,uint32_t kind,
    const KuTaskDriverTicketV1* child) {
  if_lock_idle(token->driver);
  CHECK(if_parent->wait.driver==token->driver && if_parent->wait.parent_slot==token->parent_slot
      && if_parent->wait.parent_generation==token->parent_generation && if_parent->wait.epoch==token->epoch);
  if_unlock(token->driver);
  KuTaskDriverWaitSnapshotV1 state={0};
  CHECK(ku_task_driver_wait_read(token,&state)==KU_TASK_DRIVER_OK);
  CHECK(state.kind==kind && state.state==KU_TASK_DRIVER_WAIT_ARMED
      && state.child_slot==child->slot && state.child_generation==child->generation);
}
static void if_case(unsigned mode) {
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes);
  if_mode=mode; if_parent=NULL; if_child=NULL; if_ready=NULL;
  memset(if_starts,0,sizeof(if_starts)); memset(if_ids,0,sizeof(if_ids)); memset(if_frees,0,sizeof(if_frees));
  if_entry=if_await_visits=if_ready_visits=if_cleanup_visits=if_closed=if_joined=if_transferred=if_ack_seen=0;
  if_child_resumes=if_ready_resumes=0; if_scope_deadline=0;
  if_child_observer=(KuTaskControlLeaseV1){0}; if_ready_observer=(KuTaskControlLeaseV1){0};
  if_receipt=(KuTaskDriverCleanupReceiptV1){0};
  ku_task_control_atomic_store(&if_release_ready,0); ku_task_control_atomic_store(&if_release_child,0);
  ku_task_control_atomic_store(&if_release_parent,0);
  CHECK(ku_test_event_init(&if_ready_parked) && ku_test_event_init(&if_child_cleanup) && ku_test_event_init(&if_scope_closed));
  KuTaskDriverV1* driver=(KuTaskDriverV1*)calloc(1,sizeof(*driver));
  KuTaskDriverSlotV1* slots=(KuTaskDriverSlotV1*)calloc(3,sizeof(*slots));
  size_t* ring=(size_t*)calloc(3,sizeof(*ring)); CHECK(driver && slots && ring);
  size_t fixed=sizeof(*driver)+3u*(sizeof(*slots)+sizeof(*ring));
  size_t instances=sizeof(KuTaskInstance_0)+sizeof(KuTaskInstance_1)+sizeof(KuTaskInstance_2);
  CHECK(ku_task_driver_init(driver,sizeof(*driver),KU_TASK_DRIVER_ABI_VERSION,slots,3,ring,3,
      fixed+instances+if_caps[0]+if_caps[1]+if_caps[2])==KU_TASK_DRIVER_OK);
  bool gate=mode!=IF_FALSE;
  KuString local=if_owned(0u,"held"),child_input=if_owned(1u,"child"),returned=if_owned(2u,"returned");
  KuTaskValueV1 root={0};
  CHECK(ku_task_0_start_value(driver,&gate,&local,&child_input,&returned,&root)==KU_TASK_DRIVER_OK && root.tag==KU_TASK_VALUE_LIVE);
  CHECK(gate==(mode!=IF_FALSE) && if_empty_string(local) && if_empty_string(child_input) && if_empty_string(returned));
  KuTaskControlV1* control=root.owner.lease.control;
  KuTaskAdapterOutcomeV1 outcome={0};
  if (mode==IF_FALSE) {
    CHECK(ku_task_driver_wait_result(&root.ticket,&root.owner.lease,ku_task_driver_now_ms()+2000u)==KU_TASK_DRIVER_WAIT_READY);
    if_idle(driver); CHECK(if_parent==(KuTaskInstance_0*)control);
    CHECK(if_entry==1u && !if_starts[1] && !if_starts[2] && !if_await_visits && !if_closed && if_joined==1u);
    CHECK(!if_parent->drain_started && !if_parent->drain_deadline && !if_parent->scope_mode && !if_transferred);
    CHECK(!ku_task_driver_lock(driver)); KuTaskDriverSlotV1* slot=ku_task_driver_find(&root.ticket);
    CHECK(slot && !slot->scope_session_epoch && !slot->scope_wait_started && !slot->scope_session_active);
    CHECK(!ku_task_driver_unlock(driver));
  } else {
    CHECK(ku_test_event_wait(&if_ready_parked,2000u)); if_idle(driver);
    if_lock_idle(driver);
    CHECK(if_parent==(KuTaskInstance_0*)control && if_child && if_ready && if_starts[1]==1u && if_starts[2]==1u);
    CHECK(if_parent->frame.header.state==@AWAIT@u && if_parent->frame.header.status==KU_TASK_FRAME_PENDING);
    CHECK((uintptr_t)if_parent->frame.s_@HELD@.ptr==if_ids[0] && (uintptr_t)if_child->frame.s_0.ptr==if_ids[1]);
    CHECK(if_parent->frame.s_@CHILD@.tag==KU_TASK_VALUE_LIVE && if_parent->frame.s_@READY_TASK@.tag==KU_TASK_VALUE_LIVE);
    KuTaskDriverWaitTokenV1 waiting=if_parent->wait;
    CHECK(waiting.driver && !if_parent->payload_initialized && !if_parent->frame.header.result_initialized);
    if_unlock(driver);
    FixtureLedger before=fixture_ledger();
    for (unsigned repeat=0;repeat<3u;repeat++) {
      CHECK(ku_task_driver_wake(&root.ticket)==KU_TASK_DRIVER_OK); if_idle(driver);
      if_wait_kind(&waiting,KU_TASK_DRIVER_WAIT_KIND_RESULT,&if_ready->ticket);
      if_lock_idle(driver);
      CHECK(if_entry==1u && if_starts[1]==1u && if_starts[2]==1u && if_ready_resumes==1u && if_child_resumes==1u);
      CHECK(if_await_visits==repeat+1u && !if_joined && !if_transferred);
      CHECK(!if_frees[0] && !if_frees[1] && !if_frees[2]);
      if_unlock(driver);
      FixtureLedger unchanged=fixture_ledger();
      CHECK(unchanged.allocations==before.allocations && unchanged.bytes==before.bytes && unchanged.calls==before.calls);
    }
    KuTaskDriverSnapshotV1 stable=if_idle(driver); CHECK(if_idle(driver).polls==stable.polls);
    ku_task_control_atomic_store(&if_release_ready,1); CHECK(ku_task_driver_wake(&if_ready->ticket)==KU_TASK_DRIVER_OK);
    CHECK(ku_test_event_wait(&if_child_cleanup,2000u)); if_idle(driver);
    if_lock_idle(driver);
    CHECK(if_await_visits==4u && if_ready_resumes==2u && if_transferred==1u && !if_joined);
    CHECK(ku_task_control_atomic_load(&if_child->control.phase)==KU_TASK_CONTROL_REQUESTED_CANCEL
        && ku_task_control_cleanup_deadline(&if_child->control)==if_scope_deadline);
    CHECK(ku_task_control_atomic_load(&if_ready->control.phase)==KU_TASK_CONTROL_COMPLETED
        && ku_task_control_atomic_load(&if_ready->control.payload)==KU_TASK_CONTROL_PAYLOAD_TAKEN
        && if_ready->control.frame_destroyed);
    CHECK(if_parent->frame.header.status==KU_TASK_FRAME_SCOPE_REQUEST && if_parent->frame.header.state==@DRAIN@u);
    CHECK(!(if_parent->frame.header.initialized&UINT64_C(@TASK_MASK@)) && !if_frees[0] && !if_frees[1] && !if_frees[2]);
    KuTaskDriverScopeTokenV1 scope=if_parent->scope_token; waiting=if_parent->wait;
    CHECK(scope.driver && scope.epoch && if_parent->scope_issued_mask);
    CHECK(if_parent->scope_expected_mask==(UINT64_C(1)<<@RECEIPT@)
        && if_parent->scope_issued_mask==if_parent->scope_expected_mask);
    if_unlock(driver);
    for (unsigned repeat=0;repeat<2u;repeat++) {
      CHECK(ku_task_driver_wake(&root.ticket)==KU_TASK_DRIVER_OK); if_idle(driver);
      if_wait_kind(&waiting,KU_TASK_DRIVER_WAIT_KIND_CLEANUP_ACK,&if_child->ticket);
      if_lock_idle(driver);
      CHECK(if_parent->scope_token.epoch==scope.epoch && if_transferred==1u && if_await_visits==4u);
      CHECK(if_starts[1]==1u && if_starts[2]==1u && !if_joined && !if_frees[0] && !if_frees[2]);
      if_unlock(driver);
    }
    if_pending(&root);
    if (mode==IF_NORMAL) {
      ku_task_control_atomic_store(&if_release_child,1); CHECK(ku_task_driver_wake(&if_child->ticket)==KU_TASK_DRIVER_OK);
      CHECK(ku_test_event_wait(&if_scope_closed,2000u)); if_idle(driver);
      if_lock_idle(driver);
      CHECK(if_closed==1u && !if_ready_visits && !if_joined && !if_frees[0] && if_frees[1]==1u && !if_frees[2]);
      CHECK(!if_parent->drain_started && !if_parent->drain_deadline && !if_parent->scope_token.driver);
      if_unlock(driver);
      if_pending(&root); if_lock_idle(driver); CHECK(!if_ready_visits && !if_frees[0]); if_unlock(driver);
      ku_task_control_atomic_store(&if_release_parent,1); CHECK(ku_task_driver_wake(&root.ticket)==KU_TASK_DRIVER_OK);
      CHECK(ku_task_driver_wait_result(&root.ticket,&root.owner.lease,ku_task_driver_now_ms()+2000u)==KU_TASK_DRIVER_WAIT_READY);
      if_idle(driver);
      CHECK(if_ready_visits==1u && if_joined==1u && if_frees[0]==1u && if_frees[1]==1u && !if_frees[2]);
    } else {
      CHECK(mode==IF_CANCEL);
      CHECK(ku_task_driver_request_cancel(&root.ticket,&root.owner.lease,KU_TASK_CONTROL_CANCELLED,if_scope_deadline)==KU_TASK_CONTROL_OK);
      if_idle(driver);
      if_lock_idle(driver);
      CHECK(ku_task_control_atomic_load(&control->phase)==KU_TASK_CONTROL_REQUESTED_CANCEL
          && ku_task_control_cleanup_deadline(control)==if_scope_deadline);
      CHECK(if_cleanup_visits==1u && !if_closed && !if_joined && if_frees[0]==1u && !if_frees[1] && if_frees[2]==1u);
      CHECK(if_parent->scope_mode==2u && if_parent->scope_token.epoch==scope.epoch
          && if_parent->scope_expected_mask==(UINT64_C(1)<<@RECEIPT@)
          && if_parent->scope_issued_mask==if_parent->scope_expected_mask);
      if_unlock(driver);
      if_wait_kind(&waiting,KU_TASK_DRIVER_WAIT_KIND_CLEANUP_ACK,&if_child->ticket);
      CHECK(ku_task_driver_cleanup_receipt_read(&if_receipt)==KU_TASK_DRIVER_PENDING);
      if_pending(&root);
      ku_task_control_atomic_store(&if_release_child,1); CHECK(ku_task_driver_wake(&if_child->ticket)==KU_TASK_DRIVER_OK);
      if_idle(driver);
      CHECK(ku_task_driver_cleanup_receipt_read(&if_receipt)==KU_TASK_DRIVER_CLEANUP_ACK);
      CHECK(ku_task_control_atomic_load(&control->phase)==KU_TASK_CONTROL_CANCELLED
          && ku_task_control_cleanup_deadline(control)==if_scope_deadline);
      CHECK(ku_task_value_take(&root,NULL,&outcome)==KU_TASK_CONTROL_CANCELLED && ku_task_outcome_empty(&outcome,4u));
      CHECK(ku_task_driver_wake(&root.ticket)==KU_TASK_DRIVER_OK); if_idle(driver);
      CHECK(if_cleanup_visits==1u && !if_closed && !if_joined && !if_ready_visits);
    }
  }
  if (mode!=IF_CANCEL) {
    CHECK(ku_task_control_atomic_load(&control->phase)==KU_TASK_CONTROL_COMPLETED);
    CHECK(ku_task_value_take(&root,NULL,&outcome)==KU_TASK_CONTROL_OK); if_idle(driver);
    CHECK(outcome.result_kind==4u && outcome.exit_class==KU_TASK_EXIT_USER_RESULT
        && !outcome.has_cleanup_deadline && !outcome.cleanup_deadline);
    CHECK(outcome.value.string.ok && (uintptr_t)outcome.value.string.value.ptr==if_ids[2]
        && outcome.value.string.value.len==8u && !memcmp(outcome.value.string.value.ptr,"returned",8u));
    CHECK(if_frees[0]==1u && if_frees[1]==1u && !if_frees[2]);
  }
  CHECK(control->frame_destroyed && !if_parent->frame_initialized && !if_parent->payload_initialized);
  CHECK(if_parent->frame.header.status==KU_TASK_FRAME_DESTROYED && !if_parent->frame.header.initialized
      && !if_parent->frame.header.result_initialized);
  CHECK(if_empty_result(if_parent->payload) && if_empty_result(if_parent->frame.result));
  ku_task_outcome_drop(&outcome); CHECK(ku_task_outcome_empty(&outcome,4u));
  CHECK(if_frees[0]==1u && if_frees[1]==1u && if_frees[2]==1u);
  if (if_child_observer.control) {
    CHECK(if_child->control.frame_destroyed && !if_child->frame_initialized && !if_child->payload_initialized);
    CHECK(ku_task_control_atomic_load(&if_child->control.phase)==KU_TASK_CONTROL_CANCELLED);
    CHECK(ku_task_control_cleanup_deadline(&if_child->control)==if_scope_deadline);
    CHECK(ku_task_driver_cleanup_receipt_read(&if_receipt)==KU_TASK_DRIVER_CLEANUP_ACK);
    CHECK(!ku_task_control_atomic_load(&if_child->control.lifecycle_pin));
    CHECK(ku_task_control_lease_release(&if_child_observer)==KU_TASK_CONTROL_OK);
  }
  if (if_ready_observer.control) {
    CHECK(!ku_task_control_atomic_load(&if_ready->control.lifecycle_pin));
    CHECK(ku_task_control_lease_release(&if_ready_observer)==KU_TASK_CONTROL_OK);
  }
  uint64_t deadline=ku_task_driver_now_ms()+2000u;
  CHECK(ku_task_value_drop(&root,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_shutdown(driver,deadline)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 empty=if_idle(driver);
  CHECK(!empty.resident && !empty.reserved_bytes && !empty.queued && !empty.running && !empty.building);
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
  CHECK(ku_test_event_destroy(&if_ready_parked) && ku_test_event_destroy(&if_child_cleanup) && ku_test_event_destroy(&if_scope_closed));
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes && !fixture_ledger().overflow);
}
int main(void) {
  CHECK(KU_TASK_FRAME_ABI_VERSION==4u && KU_TASK_CONTROL_ABI_VERSION==2u && KU_TASK_DRIVER_ABI_VERSION==6u);
  ku_task_control_atomic_init(&if_release_ready,0); ku_task_control_atomic_init(&if_release_child,0);
  ku_task_control_atomic_init(&if_release_parent,0);
  if_case(IF_FALSE); if_case(IF_NORMAL); if_case(IF_CANCEL);
  puts("source-if-owned-pending-ok"); return 0;
}
"#;
