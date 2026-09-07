//! A successful inner cleanup must not renew a runtime unwind's deadline.
//! This uses the existing Print failure path, independently of new arithmetic.
//! Owned input comes from the legitimate typed host factory, not heap syntax.
#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;
#[path = "support/native_task_ledger.rs"]
mod native_task_ledger;

use ku::{backend::c, checker::Checker, ir::task_lower, lexer::Lexer, parser::Parser};
use native_allocation_harness::ALLOCATION_HOOK;
use native_harness::{
    compile_harness, run_bounded, TempDir, NATIVE_THREAD_LIFECYCLE_HARNESS, RUN_LIMITS, RUN_TIMEOUT,
};
use native_task_ledger::{LEDGER_LOCK, LOCKED_ALLOCATIONS};
use std::{fs, process::Command};

fn replace_once(source: String, anchor: &str, replacement: &str) -> String {
    assert_eq!(source.matches(anchor).count(), 1, "deadline hook: {anchor}");
    source.replacen(anchor, replacement, 1)
}

#[test]
fn native_task_runtime_failure_keeps_original_deadline_through_successful_inner_drains() {
    let source = r#"
async fn Grand(value: str): int! {
    held = Held(3)
    parent = Parent(value)
    return await parent
}
async fn Parent(value: str): int! {
    held = Held(2)
    leaf = Leaf(value)
    return await leaf
}
async fn Leaf(value: str): int! {
    held = Held(1)
    print(value)
    println("BAD")
    return ok(7)
}
async fn Held(id: int): int! { return ok(id) }
async fn main(): null! { return ok(null) }
"#;
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let native = task_lower::lower_program(&ast).unwrap();
    for (id, name) in ["Grand", "Parent", "Leaf", "Held", "main"]
        .iter()
        .enumerate()
    {
        assert_eq!(native.tasks.functions[id].name, *name);
    }
    let generated = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
    for forbidden in ["run_source", "const SOURCE", "Task.new", "task.spawn"] {
        assert!(!generated.contains(forbidden));
    }
    let instrumentation = format!(
        "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\n{LOCKED_ALLOCATIONS}\n{IO_HOOK}\nstatic uint32_t fixture_hold(void*,int);\nstatic void fixture_worker_exited(void);\n"
    );
    let generated = replace_once(
        generated,
        "typedef struct KuString {",
        &format!("{instrumentation}typedef struct KuString {{"),
    );
    let generated = replace_once(
        generated,
        "static uint64_t ku_task_driver_now_ms(void) {",
        &format!("{CLOCK_HOOK}\nstatic uint64_t fixture_original_now(void) {{"),
    );
    let generated = replace_once(
        generated,
        "static uint32_t ku_task_3_resume(void* raw) {",
        "static uint32_t ku_task_3_resume(void* raw) {\n  uint32_t held=fixture_hold(raw,0);\n  if (held!=UINT32_MAX) return held;",
    );
    let generated = replace_once(
        generated,
        "static uint32_t ku_task_3_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {",
        "static uint32_t ku_task_3_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {\n  uint32_t held=fixture_hold(raw,1);\n  if (held!=UINT32_MAX) return held;",
    );
    let generated = replace_once(
        generated,
        "  /* No further driver or task storage access after this point. */",
        "  /* No further driver or task storage access after this point. */\n  fixture_worker_exited();",
    );
    let mut generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_unmodified_source_main(void) {",
    );
    generated.push_str(C_MAIN);
    let directory = TempDir::new("native-task-runtime-deadline");
    let path = directory.path().join("program.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute native runtime deadline inheritance"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("runtime deadline fixture ends under the real process watchdog");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-runtime-deadline-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const IO_HOOK: &str = r#"
static unsigned fixture_writes;
static size_t fixture_write(const void* data,size_t size,size_t count,FILE* stream) {
  if (stream==stdout) {
    CHECK(size==1u && count==7u && !memcmp(data,"payload",7u));
    fixture_writes++;
    return 0; /* Only inject the I/O fault, not its language error mapping. */
  }
  return fwrite(data,size,count,stream);
}
#define fwrite fixture_write
"#;
const CLOCK_HOOK: &str = r#"
static KuTaskControlDeadlineV1 fixture_clock;
static uint64_t fixture_original_now(void);
static uint64_t ku_task_driver_now_ms(void) {
  uint64_t now=ku_task_control_deadline_load(&fixture_clock);
  return now ? now : fixture_original_now();
}
"#;
const C_MAIN: &str = r#"
static KuAtomicRefcount fixture_hold_mask;
static KuTestEvent fixture_exit_event;
static unsigned fixture_cleanup_calls[3];
static void fixture_worker_exited(void) { CHECK(ku_test_event_set(&fixture_exit_event)); }
static uint32_t fixture_hold(void* raw,int cleanup) {
  KuTaskInstance_3* instance=(KuTaskInstance_3*)raw;
  int64_t id=instance->frame.s_0;
  CHECK(id>=1 && id<=3);
  if (cleanup) fixture_cleanup_calls[id-1]++;
  if (!cleanup || (ku_task_control_atomic_load(&fixture_hold_mask)&(1u<<(id-1)))) {
    /* Pause only progress. Real generated cleanup later destroys the frame and
     * the real driver publishes the ACK; no fixture fabricates either one. */
    CHECK(ku_task_driver_set_intent(&instance->ticket,KU_TASK_DRIVER_WAIT)==KU_TASK_DRIVER_OK);
    return KU_TASK_CONTROL_PENDING;
  }
  return UINT32_MAX;
}
static KuTaskDriverSnapshotV1 fixture_idle(KuTaskDriverV1* driver) {
  /* Observation wait, never a new cleanup deadline. A frozen future test clock
   * is bounded independently by the real process watchdog. */
  CHECK(ku_task_driver_wait_idle(driver,ku_task_driver_now_ms()+2000u)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot={0};
  CHECK(ku_task_driver_snapshot(driver,&snapshot)==KU_TASK_DRIVER_OK);
  CHECK(!snapshot.fault && !snapshot.running && !snapshot.queued && !snapshot.building);
  CHECK(snapshot.worker_waiting || snapshot.worker_exited);
  return snapshot;
}
static KuTaskDriverCleanupReceiptV1 fixture_receipt(KuTaskDriverCleanupReceiptV1* receipts,
    size_t count,uint64_t mask,const KuTaskDriverTicketV1* held) {
  KuTaskDriverCleanupReceiptV1 out={0}; size_t matches=0;
  for (size_t i=0;i<count;i++) if ((mask&(UINT64_C(1)<<i)) && receipts[i].slot==held->slot
      && receipts[i].generation==held->generation) { out=receipts[i]; matches++; }
  CHECK(matches==1u && out.driver==held->driver);
  CHECK(ku_task_driver_cleanup_receipt_read(&out)==KU_TASK_DRIVER_PENDING);
  return out;
}
static void fixture_text(KuString value,const char* expected) {
  size_t n=strlen(expected);
  CHECK(value.len==n && (!n || (value.ptr && !memcmp(value.ptr,expected,n))));
  CHECK(value.storage==KU_STRING_STATIC);
}
static void fixture_held_deadline(KuTaskInstance_3* held,uint64_t deadline) {
  CHECK(ku_task_control_atomic_load(&held->control.phase)==KU_TASK_CONTROL_REQUESTED_CANCEL);
  CHECK(ku_task_control_cleanup_deadline(&held->control)==deadline);
}
int main(void) {
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes);
  ku_task_control_deadline_init(&fixture_clock);
  ku_task_control_deadline_store(&fixture_clock,ku_test_real_now_ms()+100000u);
  ku_task_control_atomic_init(&fixture_hold_mask,7u);
  CHECK(ku_test_event_init(&fixture_exit_event));
  KuTaskDriverV1* driver=(KuTaskDriverV1*)calloc(1,sizeof(*driver));
  KuTaskDriverSlotV1* slots=(KuTaskDriverSlotV1*)calloc(6,sizeof(*slots));
  size_t* ring=(size_t*)calloc(6,sizeof(*ring)); CHECK(driver && slots && ring);
  size_t fixed=sizeof(*driver)+6u*(sizeof(*slots)+sizeof(*ring));
  size_t instances=sizeof(KuTaskInstance_0)+sizeof(KuTaskInstance_1)+sizeof(KuTaskInstance_2)+3u*sizeof(KuTaskInstance_3);
  CHECK(ku_task_driver_init(driver,sizeof(*driver),KU_TASK_DRIVER_ABI_VERSION,
      slots,6,ring,6,fixed+instances+31u)==KU_TASK_DRIVER_OK);
  uint8_t* buffer=(uint8_t*)malloc(31u); CHECK(buffer); memcpy(buffer,"payload",7u);
  KuString input={buffer,7u,31u,KU_STRING_OWNED};
  KuTaskValueV1 root={0};
  CHECK(ku_task_0_start_value(driver,&input,&root)==KU_TASK_DRIVER_OK && !input.ptr);
  KuTaskDriverSnapshotV1 first=fixture_idle(driver);
  CHECK(first.resident==6u && first.reserved_bytes==instances+31u && fixture_writes==1u);
  /* Real leases keep observed objects alive after their source owners are
   * consumed by Await. Read generated fields only after worker idle. */
  KuTaskControlLeaseV1 observers[5]={{0}};
  KuTaskInstance_0* grand=(KuTaskInstance_0*)root.owner.lease.control;
  KuTaskInstance_1* parent=NULL; KuTaskInstance_2* leaf=NULL;
  KuTaskInstance_3* held[3]={NULL,NULL,NULL}; size_t seen=0;
  CHECK(!ku_task_driver_lock(driver));
  for (size_t i=0;i<6;i++) {
    KuTaskDriverSlotV1* slot=&slots[i]; CHECK(slot->binding);
    if (slot->binding==&grand->control) continue;
    CHECK(seen<5u && ku_task_control_lease_retain(&slot->driver_lease,&observers[seen])==KU_TASK_CONTROL_OK);
    KuTaskControlV1* control=observers[seen++].control;
    if (control->operations.resume==ku_task_1_resume) { CHECK(!parent); parent=(KuTaskInstance_1*)control; }
    else if (control->operations.resume==ku_task_2_resume) { CHECK(!leaf); leaf=(KuTaskInstance_2*)control; }
    else {
      CHECK(control->operations.resume==ku_task_3_resume);
      KuTaskInstance_3* item=(KuTaskInstance_3*)control;
      CHECK(item->frame.s_0>=1 && item->frame.s_0<=3 && !held[item->frame.s_0-1]);
      held[item->frame.s_0-1]=item;
    }
  }
  CHECK(!ku_task_driver_unlock(driver));
  CHECK(seen==5u && parent && leaf && held[0] && held[1] && held[2]);
  CHECK(!grand->frame.s_0.ptr && !parent->frame.s_0.ptr && !leaf->frame.s_0.ptr);
  CHECK(leaf->payload_initialized && !leaf->payload.ok && leaf->exit_class==KU_TASK_EXIT_RUNTIME_FAILURE);
  fixture_text(leaf->payload.error.domain,"io"); fixture_text(leaf->payload.error.code,"write_failed");
  fixture_text(leaf->payload.error.message,"output write failed");
  uint64_t original=ku_task_driver_now_ms()+1000u;
  CHECK(leaf->drain_started && leaf->drain_deadline==original);
  CHECK(leaf->has_cleanup_deadline && leaf->cleanup_deadline==original);
  CHECK(!parent->drain_started && !grand->drain_started);
  FixtureLedger after_io=fixture_ledger();
  CHECK(after_io.allocations==9u && after_io.bytes==fixed+instances && !after_io.overflow);
  KuTaskDriverCleanupReceiptV1 receipts[3]={{0}};
  receipts[0]=fixture_receipt(leaf->receipts,sizeof(leaf->receipts)/sizeof(leaf->receipts[0]),leaf->receipt_mask,&held[0]->ticket);
  fixture_held_deadline(held[0],original);
  /* Each successful inner ACK is deliberately later than the first scope's
   * creation. Recomputing now+budget at either Await would change D. */
  ku_task_control_deadline_store(&fixture_clock,original-900u);
  ku_task_control_atomic_store(&fixture_hold_mask,6u);
  CHECK(ku_task_driver_wake(&held[0]->ticket)==KU_TASK_DRIVER_OK); fixture_idle(driver);
  CHECK(ku_task_driver_cleanup_receipt_read(&receipts[0])==KU_TASK_DRIVER_CLEANUP_ACK);
  CHECK(ku_task_control_atomic_load(&leaf->control.phase)==KU_TASK_CONTROL_FAILED && leaf->control.frame_destroyed);
  CHECK(parent->payload_initialized && !parent->payload.ok && parent->exit_class==KU_TASK_EXIT_RUNTIME_FAILURE);
  CHECK(parent->frame.header.has_exit_deadline && parent->frame.header.exit_deadline==original);
  CHECK(parent->drain_started && parent->drain_deadline==original && parent->has_cleanup_deadline && parent->cleanup_deadline==original);
  CHECK(!grand->drain_started);
  receipts[1]=fixture_receipt(parent->receipts,sizeof(parent->receipts)/sizeof(parent->receipts[0]),parent->receipt_mask,&held[1]->ticket);
  fixture_held_deadline(held[1],original);
  ku_task_control_deadline_store(&fixture_clock,original-800u);
  ku_task_control_atomic_store(&fixture_hold_mask,4u);
  CHECK(ku_task_driver_wake(&held[1]->ticket)==KU_TASK_DRIVER_OK); fixture_idle(driver);
  CHECK(ku_task_driver_cleanup_receipt_read(&receipts[1])==KU_TASK_DRIVER_CLEANUP_ACK);
  CHECK(ku_task_control_atomic_load(&parent->control.phase)==KU_TASK_CONTROL_FAILED && parent->control.frame_destroyed);
  CHECK(grand->payload_initialized && !grand->payload.ok && grand->exit_class==KU_TASK_EXIT_RUNTIME_FAILURE);
  CHECK(grand->frame.header.has_exit_deadline && grand->frame.header.exit_deadline==original);
  CHECK(grand->drain_started && grand->drain_deadline==original && grand->has_cleanup_deadline && grand->cleanup_deadline==original);
  fixture_text(grand->payload.error.domain,"io"); fixture_text(grand->payload.error.code,"write_failed");
  receipts[2]=fixture_receipt(grand->receipts,sizeof(grand->receipts)/sizeof(grand->receipts[0]),grand->receipt_mask,&held[2]->ticket);
  fixture_held_deadline(held[2],original);
  CHECK(fixture_writes==1u && fixture_ledger().calls==after_io.calls);
  /* The actual timer scan must expire Grand at the original D, not a budget
   * renewed by Parent/Grand after the successful inner cleanup ACKs. */
  CHECK(!ku_task_driver_lock(driver));
  ku_task_control_deadline_store(&fixture_clock,original);
  (void)ku_task_driver_deadlines_locked(driver,original); ku_task_driver_signal(driver);
  CHECK(!ku_task_driver_unlock(driver)); fixture_idle(driver);
  CHECK(ku_task_control_atomic_load(&grand->control.phase)==KU_TASK_CONTROL_FAILED);
  CHECK(grand->drain_failure==KU_TASK_DRIVER_CLEANUP_TIMEOUT && grand->drain_deadline==original);
  CHECK(ku_task_driver_cleanup_receipt_read(&receipts[2])==KU_TASK_DRIVER_PENDING);
  KuTaskAdapterOutcomeV1 outcome={0};
  CHECK(ku_task_value_take(&root,NULL,&outcome)==KU_TASK_CONTROL_OK);
  CHECK(outcome.result_kind==1u && !outcome.value.integer.ok && outcome.exit_class==KU_TASK_EXIT_RUNTIME_FAILURE);
  CHECK(outcome.has_cleanup_deadline && outcome.cleanup_deadline==original);
  fixture_text(outcome.value.integer.error.domain,"task");
  fixture_text(outcome.value.integer.error.code,"shutdown_timeout");
  fixture_idle(driver); size_t polls=fixture_idle(driver).polls;
  for (size_t i=0;i<4;i++) CHECK(fixture_idle(driver).polls==polls);
  /* A late real cleanup permits eventual reclamation, never deadline success. */
  ku_task_control_atomic_store(&fixture_hold_mask,0);
  CHECK(ku_task_driver_wake(&held[2]->ticket)==KU_TASK_DRIVER_OK); fixture_idle(driver);
  CHECK(ku_task_driver_cleanup_receipt_read(&receipts[2])==KU_TASK_DRIVER_CLEANUP_ACK);
  for (size_t i=0;i<3;i++) {
    CHECK(fixture_cleanup_calls[i]>=2u && fixture_cleanup_calls[i]<=3u);
    CHECK(ku_task_control_atomic_load(&held[i]->control.phase)==KU_TASK_CONTROL_CANCELLED);
    CHECK(ku_task_control_cleanup_deadline(&held[i]->control)==original && held[i]->control.frame_destroyed);
  }
  CHECK(ku_task_control_atomic_load(&grand->control.phase)==KU_TASK_CONTROL_FAILED);
  CHECK(outcome.exit_class==KU_TASK_EXIT_RUNTIME_FAILURE && outcome.cleanup_deadline==original);
  CHECK(fixture_writes==1u && fixture_ledger().calls==after_io.calls);
  ku_task_outcome_drop(&outcome);
  CHECK(ku_task_value_drop(&root,original)==KU_TASK_DRIVER_OK);
  for (size_t i=0;i<5;i++) CHECK(ku_task_control_lease_release(&observers[i])==KU_TASK_CONTROL_OK);
  KuTaskDriverSnapshotV1 empty=fixture_idle(driver); CHECK(!empty.resident && !empty.reserved_bytes);
  /* Exactly one original-D shutdown. Worker-tail timeout remains an error;
   * the real OS wait below only observes eventual completion for safe free. */
  CHECK(ku_task_driver_shutdown(driver,original)==KU_TASK_DRIVER_SHUTDOWN_TIMEOUT);
  CHECK(driver->shutdown_deadline==original);
  CHECK(ku_test_event_wait(&fixture_exit_event,2000));
#if defined(_WIN32)
  CHECK(WaitForSingleObject(driver->thread,2000)==WAIT_OBJECT_0);
#else
  alarm(2);
#endif
  CHECK(ku_task_driver_destroy(driver)==KU_TASK_DRIVER_OK);
#if !defined(_WIN32)
  alarm(0);
#endif
  CHECK(ku_test_event_destroy(&fixture_exit_event));
  free(ring); free(slots); free(driver);
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes && !fixture_ledger().overflow);
  puts("task-runtime-deadline-ok"); return 0;
}
"#;
