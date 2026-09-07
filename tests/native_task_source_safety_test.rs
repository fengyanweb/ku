//! Source-lowered Start/final-drain safety under controlled cleanup interleavings.
//! The host supplies a legitimate Owned ABI input; this does not open dynamic
//! allocation syntax. Only child cleanup progress and the clock are injected.
#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use ku::{backend::c, checker::Checker, ir::task_lower, lexer::Lexer, parser::Parser};
use native_allocation_harness::ALLOCATION_HOOK;
use native_harness::{
    compile_harness, run_bounded, TempDir, NATIVE_THREAD_LIFECYCLE_HARNESS, RUN_LIMITS, RUN_TIMEOUT,
};
use std::{fs, process::Command};

const SOURCE: &str = r#"
async fn Parent(value: str): str! {
    first = Child(1)
    second = Child(2)
    third = Child(3)
    return ok(value)
}
async fn Child(id: int): int! { return ok(id) }
async fn main(): null! { return ok(null) }
"#;

fn replace_once(source: String, anchor: &str, replacement: &str) -> String {
    assert_eq!(source.matches(anchor).count(), 1, "safety hook: {anchor}");
    source.replacen(anchor, replacement, 1)
}

#[test]
fn native_task_source_final_drain_cancel_tightens_all_receipts_and_drops_private_payload() {
    let ast = Parser::new(Lexer::new(SOURCE).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let native = task_lower::lower_program(&ast).unwrap();
    assert_eq!(native.tasks.functions[0].name, "Parent");
    assert_eq!(native.tasks.functions[1].name, "Child");
    let parent_mask = native.tasks.functions[0]
        .slots
        .iter()
        .enumerate()
        .filter(|(_, slot)| matches!(slot.ty, ku::ir::task::TaskSlotType::Task { .. }))
        .fold(0u64, |mask, (index, _)| mask | (1u64 << index));
    assert_ne!(parent_mask, 0);
    let generated = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
    for forbidden in ["run_source", "const SOURCE", "Task.new", "task.spawn"] {
        assert!(!generated.contains(forbidden));
    }
    for required in [
        "ku_task_1_start_value(",
        "ku_task_driver_owner_drop_receipt(",
        "ku_task_driver_cancel_receipt(",
        "ku_task_driver_cleanup_wait_arm(",
    ] {
        assert!(generated.contains(required));
    }
    let instrumentation = format!(
        "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\n{LOCKED_ALLOCATIONS}\nstatic uint32_t fixture_child_cleanup(void*);\nstatic void fixture_worker_exited(void);\n"
    );
    let source = replace_once(
        generated,
        "typedef struct KuString {",
        &format!("{instrumentation}typedef struct KuString {{"),
    );
    let source = replace_once(
        source,
        "static uint64_t ku_task_driver_now_ms(void) {",
        &format!("{CLOCK_HOOK}\nstatic uint64_t fixture_original_now(void) {{"),
    );
    let source = replace_once(
        source,
        "static uint32_t ku_task_1_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {",
        "static uint32_t ku_task_1_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {\n  uint32_t held=fixture_child_cleanup(raw);\n  if (held!=UINT32_MAX) return held;",
    );
    let source = replace_once(
        source,
        "  /* No further driver or task storage access after this point. */",
        "  /* No further driver or task storage access after this point. */\n  fixture_worker_exited();",
    );
    // The fixture is an external owner, not a replacement implementation of
    // source Start/Await or generated scope cleanup. Existing source tests run
    // the generated main; this entry controls cancellation through a real lease.
    let mut source = replace_once(
        source,
        "int main(void) {",
        "static int fixture_unmodified_source_main(void) {",
    );
    source.push_str(&C_MAIN.replace("@TASK_MASK@", &parent_mask.to_string()));
    let directory = TempDir::new("native-task-source-safety");
    let path = directory.path().join("program.c");
    fs::write(&path, source).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must run generated native Task scope safety"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("scope safety terminates under the real process watchdog");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-source-scope-safety-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const LEDGER_LOCK: &str = r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr,"source scope check line %d: %s\n",__LINE__,#c); abort(); } } while (0)
#if defined(_WIN32)
static SRWLOCK fixture_allocation_lock=SRWLOCK_INIT;
static void fixture_alloc_lock(void) { AcquireSRWLockExclusive(&fixture_allocation_lock); }
static void fixture_alloc_unlock(void) { ReleaseSRWLockExclusive(&fixture_allocation_lock); }
#else
static pthread_mutex_t fixture_allocation_lock=PTHREAD_MUTEX_INITIALIZER;
static void fixture_alloc_lock(void) { CHECK(!pthread_mutex_lock(&fixture_allocation_lock)); }
static void fixture_alloc_unlock(void) { CHECK(!pthread_mutex_unlock(&fixture_allocation_lock)); }
#endif
"#;

const LOCKED_ALLOCATIONS: &str = r#"
#undef malloc
#undef calloc
#undef realloc
#undef free
static void* fixture_malloc(size_t size) { fixture_alloc_lock(); void* p=ku_perf_malloc(size); fixture_alloc_unlock(); return p; }
static void* fixture_calloc(size_t count,size_t size) { fixture_alloc_lock(); void* p=ku_perf_calloc(count,size); fixture_alloc_unlock(); return p; }
static void* fixture_realloc(void* old,size_t size) { fixture_alloc_lock(); void* p=ku_perf_realloc(old,size); fixture_alloc_unlock(); return p; }
static void fixture_free(void* p) { fixture_alloc_lock(); ku_perf_free(p); fixture_alloc_unlock(); }
typedef struct FixtureLedger { size_t allocations,bytes,calls; int overflow; } FixtureLedger;
static FixtureLedger fixture_ledger(void) {
  fixture_alloc_lock(); FixtureLedger out={ku_perf_live_allocations,ku_perf_live_bytes,ku_perf_calls,ku_perf_overflow};
  fixture_alloc_unlock(); return out;
}
#define malloc fixture_malloc
#define calloc fixture_calloc
#define realloc fixture_realloc
#define free fixture_free
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
static KuAtomicRefcount fixture_hold;
static KuAtomicRefcount fixture_cleanup_calls;
static KuTestEvent fixture_exit_event;
static int fixture_exit_observing;
static void fixture_worker_exited(void) {
  if (fixture_exit_observing) CHECK(ku_test_event_set(&fixture_exit_event));
}
typedef struct FixtureDriver {
  KuTaskDriverV1* driver;
  KuTaskDriverSlotV1* slots;
  size_t* ring;
} FixtureDriver;
static uint32_t fixture_child_cleanup(void* raw) {
  KuTaskInstance_1* instance=(KuTaskInstance_1*)raw;
  ku_task_control_atomic_store(&fixture_cleanup_calls,ku_task_control_atomic_load(&fixture_cleanup_calls)+1u);
  if (!ku_task_control_atomic_load(&fixture_hold)) return UINT32_MAX;
  /* Only defer the real cleanup. No fake ACK, frame destruction, transfer or
   * outcome is supplied by the fixture. Release resumes the original body. */
  CHECK(ku_task_driver_set_intent(&instance->ticket,KU_TASK_DRIVER_WAIT)==KU_TASK_DRIVER_OK);
  return KU_TASK_CONTROL_PENDING;
}
static KuTaskDriverSnapshotV1 fixture_snapshot(FixtureDriver* runtime) {
  KuTaskDriverSnapshotV1 snapshot={0};
  CHECK(ku_task_driver_snapshot(runtime->driver,&snapshot)==KU_TASK_DRIVER_OK);
  CHECK(!snapshot.fault && snapshot.running<=1 && snapshot.resident<=4);
  return snapshot;
}
static void fixture_idle(FixtureDriver* runtime) {
  /* The virtual clock is frozen ahead of real time. The real 20-second outer
   * watchdog remains authoritative if this condition wait loses progress. */
  CHECK(ku_task_driver_wait_idle(runtime->driver,ku_task_driver_now_ms()+2000u)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot=fixture_snapshot(runtime);
  CHECK(!snapshot.running && !snapshot.queued && (snapshot.worker_waiting || snapshot.worker_exited));
}
static void fixture_init(FixtureDriver* runtime) {
  memset(runtime,0,sizeof(*runtime));
  runtime->driver=(KuTaskDriverV1*)calloc(1,sizeof(*runtime->driver));
  runtime->slots=(KuTaskDriverSlotV1*)calloc(4,sizeof(*runtime->slots));
  runtime->ring=(size_t*)calloc(4,sizeof(*runtime->ring));
  CHECK(runtime->driver && runtime->slots && runtime->ring);
  size_t fixed=sizeof(*runtime->driver)+4*(sizeof(*runtime->slots)+sizeof(*runtime->ring));
  size_t tasks=sizeof(KuTaskInstance_0)+3*sizeof(KuTaskInstance_1)+31u;
  CHECK(ku_task_driver_init(runtime->driver,sizeof(*runtime->driver),KU_TASK_DRIVER_ABI_VERSION,
      runtime->slots,4,runtime->ring,4,fixed+tasks)==KU_TASK_DRIVER_OK);
  fixture_idle(runtime);
}
static void fixture_finish(FixtureDriver* runtime,uint64_t deadline) {
  CHECK(ku_task_driver_shutdown(runtime->driver,deadline)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot=fixture_snapshot(runtime);
  CHECK(!snapshot.resident && !snapshot.reserved_bytes && !snapshot.queued && !snapshot.running && !snapshot.building);
#if defined(_WIN32)
  CHECK(WaitForSingleObject(runtime->driver->thread,2000)==WAIT_OBJECT_0);
#endif
  CHECK(ku_task_driver_destroy(runtime->driver)==KU_TASK_DRIVER_OK);
  free(runtime->ring); free(runtime->slots); free(runtime->driver);
  FixtureLedger ledger=fixture_ledger();
  CHECK(!ledger.allocations && !ledger.bytes && !ledger.overflow);
}
static void fixture_check_children(FixtureDriver* runtime,KuTaskControlLeaseV1* observers,
    KuTaskDriverTicketV1* tickets,uint64_t deadline) {
  CHECK(!ku_task_driver_lock(runtime->driver));
  for (size_t i=0;i<3;i++) {
    KuTaskDriverSlotV1* slot=&runtime->slots[tickets[i].slot];
    CHECK(slot->generation==tickets[i].generation && slot->binding==observers[i].control);
    CHECK(slot->owner_location==KU_TASK_DRIVER_OWNER_RELEASED);
    CHECK(ku_task_control_atomic_load(&observers[i].control->phase)==KU_TASK_CONTROL_REQUESTED_CANCEL);
    CHECK(ku_task_control_cleanup_deadline(observers[i].control)==deadline);
    CHECK(slot->cancel_deadline==deadline && !slot->cleanup_acked_generation);
  }
  CHECK(!ku_task_driver_unlock(runtime->driver));
}
static void fixture_scope(int mode) {
  FixtureLedger zero=fixture_ledger(); CHECK(!zero.allocations && !zero.bytes);
  ku_task_control_deadline_store(&fixture_clock,ku_test_real_now_ms()+100000u);
  ku_task_control_atomic_store(&fixture_hold,1);
  ku_task_control_atomic_store(&fixture_cleanup_calls,0);
  FixtureDriver runtime; fixture_init(&runtime);
  uint8_t* buffer=(uint8_t*)malloc(31u); CHECK(buffer); memcpy(buffer,"payload",7u);
  KuString input={buffer,7u,31u,KU_STRING_OWNED};
  KuTaskValueV1 parent={0};
  CHECK(ku_task_0_start_value(runtime.driver,&input,&parent)==KU_TASK_DRIVER_OK);
  CHECK(parent.tag==KU_TASK_VALUE_LIVE && !input.ptr);
  KuTaskControlLeaseV1 cancellation={0};
  CHECK(ku_task_control_lease_retain(&parent.owner.lease,&cancellation)==KU_TASK_CONTROL_OK);
  fixture_idle(&runtime);
  KuTaskInstance_0* instance=(KuTaskInstance_0*)cancellation.control;
  /* Idle and frozen time ensure no callback mutates these generated fields.
   * Parent's real owner/observer keep storage alive throughout inspection. */
  CHECK(instance->payload_initialized && instance->payload.ok);
  CHECK(instance->payload.value.ptr==buffer && instance->payload.value.capacity==31u);
  CHECK(instance->frame.header.status==KU_TASK_FRAME_READY);
  CHECK(!(instance->frame.header.initialized & UINT64_C(@TASK_MASK@)));
  CHECK(ku_task_control_atomic_load(&cancellation.control->phase)==KU_TASK_CONTROL_LIVE);
  CHECK(ku_task_control_atomic_load(&cancellation.control->payload)==KU_TASK_CONTROL_PAYLOAD_EMPTY);
  CHECK(instance->drain_started && instance->drain_deadline_published);
  uint64_t original=ku_task_driver_now_ms()+1000u;
  CHECK(instance->drain_deadline==original && instance->drain_published_deadline==original);
  KuTaskDriverWaitSnapshotV1 wait={0};
  CHECK(ku_task_driver_wait_read(&instance->wait,&wait)==KU_TASK_DRIVER_OK);
  CHECK(wait.kind==KU_TASK_DRIVER_WAIT_KIND_CLEANUP_ACK && wait.state==KU_TASK_DRIVER_WAIT_ARMED);
  KuTaskDriverWaitTokenV1 token=instance->wait;
  KuTaskControlLeaseV1 children[3]={{0}};
  KuTaskDriverTicketV1 tickets[3]={{0}};
  KuTaskDriverCleanupReceiptV1 receipts[3]={{0}};
  size_t count=0;
  CHECK(!ku_task_driver_lock(runtime.driver));
  for (size_t i=0;i<sizeof(instance->receipts)/sizeof(instance->receipts[0]);i++) {
    if (!(instance->receipt_mask & (UINT64_C(1)<<i))) continue;
    CHECK(count<3); receipts[count]=instance->receipts[i];
    KuTaskDriverSlotV1* child=&runtime.slots[receipts[count].slot];
    CHECK(child->driver_lease.control && child->driver_lease.control->operations.cleanup==ku_task_1_cleanup);
    CHECK(ku_task_control_lease_retain(&child->driver_lease,&children[count])==KU_TASK_CONTROL_OK);
    tickets[count]=((KuTaskInstance_1*)children[count].control)->ticket;
    count++;
  }
  CHECK(count==3); CHECK(!ku_task_driver_unlock(runtime.driver));
  fixture_check_children(&runtime,children,tickets,original);
  FixtureLedger staged=fixture_ledger();
  CHECK(staged.allocations==8u && staged.bytes==fixture_snapshot(&runtime).fixed_bytes+sizeof(KuTaskInstance_0)+3*sizeof(KuTaskInstance_1)+31u);
  CHECK(ku_task_control_atomic_load(&fixture_cleanup_calls)==3u);
  uint64_t deadline=original;
  uint32_t terminal=mode==2 ? KU_TASK_CONTROL_TIMED_OUT : KU_TASK_CONTROL_CANCELLED;
  if (mode) {
    deadline=original-300u;
    CHECK(ku_task_driver_request_cancel(&parent.ticket,&cancellation,terminal,deadline)==KU_TASK_CONTROL_OK);
    fixture_idle(&runtime);
    fixture_check_children(&runtime,children,tickets,deadline);
    CHECK(instance->drain_deadline==deadline && instance->drain_published_deadline==deadline);
    CHECK(!instance->payload_initialized && !instance->frame_initialized && instance->values_cleaned);
    FixtureLedger cancelled=fixture_ledger();
    CHECK(cancelled.allocations+1u==staged.allocations && cancelled.bytes+31u==staged.bytes);
    CHECK(cancelled.calls==staged.calls);
    CHECK(instance->wait.epoch==token.epoch && instance->wait.driver==token.driver);
    uint32_t requested=mode==2 ? KU_TASK_CONTROL_REQUESTED_TIMEOUT : KU_TASK_CONTROL_REQUESTED_CANCEL;
    CHECK(ku_task_control_atomic_load(&cancellation.control->phase)==requested);
    deadline-=200u;
    uint32_t losing=mode==2 ? KU_TASK_CONTROL_CANCELLED : KU_TASK_CONTROL_TIMED_OUT;
    CHECK(ku_task_driver_request_cancel(&parent.ticket,&cancellation,losing,deadline)==KU_TASK_CONTROL_OK);
    fixture_idle(&runtime);
    fixture_check_children(&runtime,children,tickets,deadline);
    CHECK(ku_task_control_atomic_load(&cancellation.control->phase)==requested);
    CHECK(ku_task_control_cleanup_deadline(cancellation.control)==deadline);
    CHECK(instance->drain_deadline==deadline && instance->drain_published_deadline==deadline);
    CHECK(instance->wait.epoch==token.epoch);
    CHECK(fixture_ledger().allocations==cancelled.allocations && fixture_ledger().bytes==cancelled.bytes);
    /* A later, looser request never renews either ancestor or sibling budget. */
    CHECK(ku_task_driver_request_cancel(&parent.ticket,&cancellation,losing,original+1000u)==KU_TASK_CONTROL_OK);
    fixture_idle(&runtime); fixture_check_children(&runtime,children,tickets,deadline);
  }
  size_t stable_polls=fixture_snapshot(&runtime).polls;
  for (size_t i=0;i<8;i++) {
    fixture_idle(&runtime);
    CHECK(fixture_snapshot(&runtime).polls==stable_polls);
  }
  /* One external notification per child releases the test-only barrier.
   * The original generated cleanup and ACK publication do all actual work. */
  ku_task_control_atomic_store(&fixture_hold,0);
  for (size_t i=0;i<3;i++) CHECK(ku_task_driver_wake(&tickets[i])==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_wait_result(&parent.ticket,&parent.owner.lease,original+1000u)==KU_TASK_DRIVER_WAIT_READY);
  fixture_idle(&runtime);
  for (size_t i=0;i<3;i++) {
    CHECK(ku_task_driver_cleanup_receipt_read(&receipts[i])==KU_TASK_DRIVER_CLEANUP_ACK);
    CHECK(ku_task_control_atomic_load(&children[i].control->phase)==KU_TASK_CONTROL_CANCELLED);
    CHECK(ku_task_control_cleanup_deadline(children[i].control)==deadline);
    CHECK(children[i].control->frame_destroyed && !ku_task_control_atomic_load(&children[i].control->lifecycle_pin));
  }
  KuTaskAdapterOutcomeV1 outcome={0};
  uint32_t taken=ku_task_value_take(&parent,NULL,&outcome);
  if (mode) {
    CHECK(taken==terminal && !outcome.result_kind);
    CHECK(ku_task_control_atomic_load(&cancellation.control->phase)==terminal);
  } else {
    CHECK(taken==KU_TASK_CONTROL_OK && outcome.result_kind==4u && outcome.exit_class==KU_TASK_EXIT_USER_RESULT);
    CHECK(outcome.value.string.ok && outcome.value.string.value.ptr==buffer);
    CHECK(!outcome.has_cleanup_deadline && !outcome.cleanup_deadline);
  }
  ku_task_outcome_drop(&outcome);
  CHECK(ku_task_value_drop(&parent,deadline)==KU_TASK_DRIVER_OK);
  CHECK(!parent.tag && !parent.owner.lease.control);
  CHECK(ku_task_control_lease_release(&cancellation)==KU_TASK_CONTROL_OK);
  for (size_t i=0;i<3;i++) CHECK(ku_task_control_lease_release(&children[i])==KU_TASK_CONTROL_OK);
  fixture_finish(&runtime,deadline);
}
static void fixture_scope_timeout(void) {
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes);
  CHECK(ku_test_event_init(&fixture_exit_event)); fixture_exit_observing=1;
  ku_task_control_deadline_store(&fixture_clock,ku_test_real_now_ms()+100000u);
  ku_task_control_atomic_store(&fixture_hold,1);
  ku_task_control_atomic_store(&fixture_cleanup_calls,0);
  FixtureDriver runtime; fixture_init(&runtime);
  uint8_t* buffer=(uint8_t*)malloc(31u); CHECK(buffer); memcpy(buffer,"payload",7u);
  KuString input={buffer,7u,31u,KU_STRING_OWNED};
  KuTaskValueV1 parent={0};
  CHECK(ku_task_0_start_value(runtime.driver,&input,&parent)==KU_TASK_DRIVER_OK);
  CHECK(parent.tag==KU_TASK_VALUE_LIVE && !input.ptr);
  fixture_idle(&runtime);
  KuTaskInstance_0* instance=(KuTaskInstance_0*)parent.owner.lease.control;
  CHECK(instance->payload_initialized && instance->payload.ok && instance->payload.value.ptr==buffer);
  CHECK(instance->frame.header.status==KU_TASK_FRAME_READY && instance->drain_started);
  CHECK(!(instance->frame.header.initialized & UINT64_C(@TASK_MASK@)));
  CHECK(ku_task_control_atomic_load(&instance->control.phase)==KU_TASK_CONTROL_LIVE);
  CHECK(ku_task_control_atomic_load(&instance->control.payload)==KU_TASK_CONTROL_PAYLOAD_EMPTY);
  uint64_t deadline=instance->drain_deadline;
  CHECK(deadline==ku_task_driver_now_ms()+1000u);
  KuTaskDriverWaitSnapshotV1 wait={0};
  CHECK(ku_task_driver_wait_read(&instance->wait,&wait)==KU_TASK_DRIVER_OK);
  CHECK(wait.kind==KU_TASK_DRIVER_WAIT_KIND_CLEANUP_ACK && wait.state==KU_TASK_DRIVER_WAIT_ARMED);
  KuTaskControlLeaseV1 children[3]={{0}};
  KuTaskDriverTicketV1 tickets[3]={{0}};
  KuTaskDriverCleanupReceiptV1 receipts[3]={{0}};
  size_t count=0;
  CHECK(!ku_task_driver_lock(runtime.driver));
  for (size_t i=0;i<sizeof(instance->receipts)/sizeof(instance->receipts[0]);i++) {
    if (!(instance->receipt_mask & (UINT64_C(1)<<i))) continue;
    CHECK(count<3); receipts[count]=instance->receipts[i];
    KuTaskDriverSlotV1* child=&runtime.slots[receipts[count].slot];
    CHECK(child->driver_lease.control && child->driver_lease.control->operations.cleanup==ku_task_1_cleanup);
    CHECK(ku_task_control_lease_retain(&child->driver_lease,&children[count])==KU_TASK_CONTROL_OK);
    tickets[count]=((KuTaskInstance_1*)children[count].control)->ticket;
    count++;
  }
  CHECK(count==3); CHECK(!ku_task_driver_unlock(runtime.driver));
  FixtureLedger staged=fixture_ledger(); CHECK(staged.allocations==8u);
  fixture_check_children(&runtime,children,tickets,deadline);
  /* Advance only the test clock. Run the real timer scan under its required
   * mutex; the generated adapter, not this fixture, selects the exit class. */
  CHECK(!ku_task_driver_lock(runtime.driver));
  ku_task_control_deadline_store(&fixture_clock,deadline);
  (void)ku_task_driver_deadlines_locked(runtime.driver,deadline);
  ku_task_driver_signal(runtime.driver);
  CHECK(!ku_task_driver_unlock(runtime.driver));
  fixture_idle(&runtime); /* observation only; never extends a cleanup budget */
  CHECK(ku_task_control_atomic_load(&instance->control.phase)==KU_TASK_CONTROL_FAILED);
  CHECK(instance->control.frame_destroyed && !instance->frame_initialized);
  CHECK(instance->drain_failure==KU_TASK_DRIVER_CLEANUP_TIMEOUT && instance->drain_deadline==deadline);
  CHECK(instance->exit_class==KU_TASK_EXIT_RUNTIME_FAILURE && instance->has_cleanup_deadline && instance->cleanup_deadline==deadline);
  FixtureLedger expired=fixture_ledger();
  CHECK(expired.allocations+1u==staged.allocations && expired.bytes+31u==staged.bytes && expired.calls==staged.calls);
  for (size_t i=0;i<3;i++) CHECK(ku_task_driver_cleanup_receipt_read(&receipts[i])==KU_TASK_DRIVER_PENDING);
  KuTaskAdapterOutcomeV1 outcome={0};
  CHECK(ku_task_value_take(&parent,NULL,&outcome)==KU_TASK_CONTROL_OK);
  CHECK(outcome.result_kind==4u && !outcome.value.string.ok && outcome.exit_class==KU_TASK_EXIT_RUNTIME_FAILURE);
  CHECK(outcome.has_cleanup_deadline && outcome.cleanup_deadline==deadline);
  CHECK(outcome.value.string.error.domain.len==4u && !memcmp(outcome.value.string.error.domain.ptr,"task",4u));
  CHECK(outcome.value.string.error.code.len==16u && !memcmp(outcome.value.string.error.code.ptr,"shutdown_timeout",16u));
  size_t stable_polls=fixture_snapshot(&runtime).polls;
  for (size_t i=0;i<8;i++) { fixture_idle(&runtime); CHECK(fixture_snapshot(&runtime).polls==stable_polls); }
  /* These are LATE cleanups. They permit safe eventual reclamation but cannot
   * turn the already-failed exit into success or renew its absolute budget. */
  ku_task_control_atomic_store(&fixture_hold,0);
  for (size_t i=0;i<3;i++) CHECK(ku_task_driver_wake(&tickets[i])==KU_TASK_DRIVER_OK);
  fixture_idle(&runtime);
  for (size_t i=0;i<3;i++) {
    CHECK(ku_task_driver_cleanup_receipt_read(&receipts[i])==KU_TASK_DRIVER_CLEANUP_ACK);
    CHECK(ku_task_control_atomic_load(&children[i].control->phase)==KU_TASK_CONTROL_CANCELLED);
    CHECK(ku_task_control_cleanup_deadline(children[i].control)==deadline);
  }
  CHECK(ku_task_control_atomic_load(&instance->control.phase)==KU_TASK_CONTROL_FAILED);
  CHECK(instance->drain_failure==KU_TASK_DRIVER_CLEANUP_TIMEOUT && instance->drain_deadline==deadline);
  CHECK(outcome.exit_class==KU_TASK_EXIT_RUNTIME_FAILURE && outcome.cleanup_deadline==deadline && !outcome.value.string.ok);
  CHECK(fixture_ledger().allocations==expired.allocations && fixture_ledger().bytes==expired.bytes);
  ku_task_outcome_drop(&outcome);
  CHECK(ku_task_value_drop(&parent,deadline)==KU_TASK_DRIVER_OK);
  for (size_t i=0;i<3;i++) CHECK(ku_task_control_lease_release(&children[i])==KU_TASK_CONTROL_OK);
  fixture_idle(&runtime);
  CHECK(!fixture_snapshot(&runtime).resident && !fixture_snapshot(&runtime).reserved_bytes);
  /* Exactly one shutdown with D. Its worker-tail timeout remains a failure;
   * test-only observation waits below are not another runtime cleanup budget. */
  CHECK(ku_task_driver_shutdown(runtime.driver,deadline)==KU_TASK_DRIVER_SHUTDOWN_TIMEOUT);
  CHECK(runtime.driver->shutdown_deadline==deadline);
  CHECK(ku_test_event_wait(&fixture_exit_event,2000));
#if defined(_WIN32)
  CHECK(WaitForSingleObject(runtime.driver->thread,2000)==WAIT_OBJECT_0);
#else
  alarm(2); /* Bound the OS join after the observed worker exit publication. */
#endif
  CHECK(ku_task_driver_destroy(runtime.driver)==KU_TASK_DRIVER_OK);
#if !defined(_WIN32)
  alarm(0);
#endif
  fixture_exit_observing=0; CHECK(ku_test_event_destroy(&fixture_exit_event));
  free(runtime.ring); free(runtime.slots); free(runtime.driver);
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes && !fixture_ledger().overflow);
}
int main(void) {
  CHECK(KU_TASK_FRAME_ABI_VERSION==2u && KU_TASK_DRIVER_ABI_VERSION==4u);
  ku_task_control_deadline_init(&fixture_clock);
  ku_task_control_atomic_init(&fixture_hold,0);
  ku_task_control_atomic_init(&fixture_cleanup_calls,0);
  fixture_scope(0); fixture_scope(1); fixture_scope(2);
  fixture_scope_timeout();
  puts("task-source-scope-safety-ok");
  return 0;
}
"#;

#[test]
fn native_task_source_start_rejections_and_await_owned_charge_execute_in_c() {
    let source = r#"
async fn Parent(value: str): str! {
    child = Text(value)
    return await child
}
async fn Text(value: str): str! { return ok(value) }
async fn main(): null! { return ok(null) }
"#;
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let native = task_lower::lower_program(&ast).unwrap();
    assert_eq!(native.tasks.functions[0].name, "Parent");
    assert_eq!(native.tasks.functions[1].name, "Text");
    let generated = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
    for forbidden in ["run_source", "const SOURCE", "Task.new", "task.spawn"] {
        assert!(!generated.contains(forbidden));
    }
    let instrumentation = format!(
        "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\n{LOCKED_ALLOCATIONS}\nstatic void* fixture_text_calloc(size_t,size_t);\nstatic void fixture_observe_charge(int,const void*,const void*,size_t);\n"
    );
    let generated = replace_once(
        generated,
        "typedef struct KuString {",
        &format!("{instrumentation}typedef struct KuString {{"),
    );
    // Reject only the actual child factory allocation; the source Parent must
    // still consume its argument and Await the generated inline failed Task.
    let generated = replace_once(
        generated,
        "KuTaskInstance_1* instance = (KuTaskInstance_1*)calloc(1, sizeof(*instance));",
        "KuTaskInstance_1* instance = (KuTaskInstance_1*)fixture_text_calloc(1, sizeof(*instance));",
    );
    let take_start = generated
        .find("static uint32_t ku_task_1_take_payload(void* raw, void* destination) {")
        .unwrap();
    let take_end = generated[take_start..]
        .find("static void ku_task_1_dispose(KuTaskControlV1* control, void* raw) {")
        .unwrap()
        + take_start;
    let original_take = generated[take_start..take_end].to_owned();
    let observed_take = replace_once(
        original_take.clone(),
        "    checked=ku_task_driver_transfer_charge(&instance->ticket,request->parent,payload_charge,sizeof(*instance));\n    if (checked!=KU_TASK_DRIVER_OK) return checked;",
        "    fixture_observe_charge(0,&instance->ticket,request->parent,payload_charge);\n    checked=ku_task_driver_transfer_charge(&instance->ticket,request->parent,payload_charge,sizeof(*instance));\n    if (checked!=KU_TASK_DRIVER_OK) return checked;\n    fixture_observe_charge(1,&instance->ticket,request->parent,payload_charge);",
    );
    let generated = replace_once(generated, &original_take, &observed_take);
    let mut generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_unmodified_source_main(void) {",
    );
    generated.push_str(C_REJECTIONS);
    let directory = TempDir::new("native-task-source-rejections");
    let path = directory.path().join("program.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must run source Task rejection and Owned charge paths"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("source Start rejection terminates under the real process watchdog");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-source-start-await-safety-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const C_REJECTIONS: &str = r#"
static KuAtomicRefcount fixture_calloc_failure;
static KuAtomicRefcount fixture_calloc_attempts;
typedef struct FixtureCharge {
  size_t before,after,child,parent,bytes;
  size_t observations;
} FixtureCharge;
static FixtureCharge fixture_charge;
static void* fixture_text_calloc(size_t count,size_t size) {
  ku_task_control_atomic_store(&fixture_calloc_attempts,ku_task_control_atomic_load(&fixture_calloc_attempts)+1u);
  if (ku_task_control_atomic_load(&fixture_calloc_failure)) return NULL;
  return calloc(count,size);
}
static void fixture_observe_charge(int after,const void* raw_child,const void* raw_parent,size_t bytes) {
  const KuTaskDriverTicketV1* child=(const KuTaskDriverTicketV1*)raw_child;
  const KuTaskDriverTicketV1* parent=(const KuTaskDriverTicketV1*)raw_parent;
  CHECK(child->driver==parent->driver && child->slot!=parent->slot && bytes==31u);
  KuTaskDriverV1* driver=child->driver;
  CHECK(!ku_task_driver_lock(driver));
  KuTaskDriverSlotV1* from=ku_task_driver_find(child);
  KuTaskDriverSlotV1* to=ku_task_driver_find(parent);
  CHECK(from && to && to->state==KU_TASK_DRIVER_RUNNING);
  CHECK(from->binding && ku_task_control_atomic_load(&from->binding->payload)==KU_TASK_CONTROL_PAYLOAD_TAKING);
  if (!after) {
    CHECK(!fixture_charge.observations);
    fixture_charge.before=driver->reserved_bytes;
    fixture_charge.child=from->charged_bytes;
    fixture_charge.parent=to->charged_bytes;
    fixture_charge.bytes=bytes;
    CHECK(from->charged_bytes==sizeof(KuTaskInstance_1)+bytes);
    CHECK(to->charged_bytes==sizeof(KuTaskInstance_0)+bytes);
  } else {
    CHECK(fixture_charge.observations==1u && fixture_charge.bytes==bytes);
    fixture_charge.after=driver->reserved_bytes;
    CHECK(fixture_charge.before==fixture_charge.after);
    CHECK(from->charged_bytes+bytes==fixture_charge.child);
    CHECK(to->charged_bytes==fixture_charge.parent+bytes);
    CHECK(from->charged_bytes==sizeof(KuTaskInstance_1));
  }
  fixture_charge.observations++;
  CHECK(!ku_task_driver_unlock(driver));
}
static void fixture_text(KuString value,const char* expected) {
  size_t length=strlen(expected);
  CHECK(value.len==length && (!length || (value.ptr && !memcmp(value.ptr,expected,length))));
}
static void fixture_rejection(int mode) {
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes);
  ku_task_control_atomic_store(&fixture_calloc_failure,mode==2);
  ku_task_control_atomic_store(&fixture_calloc_attempts,0);
  fixture_charge=(FixtureCharge){0};
  size_t capacity=mode==1 ? 1u : 2u;
  KuTaskDriverV1* driver=(KuTaskDriverV1*)calloc(1,sizeof(*driver));
  KuTaskDriverSlotV1* slots=(KuTaskDriverSlotV1*)calloc(capacity,sizeof(*slots));
  size_t* ring=(size_t*)calloc(capacity,sizeof(*ring)); CHECK(driver && slots && ring);
  size_t fixed=sizeof(*driver)+capacity*(sizeof(*slots)+sizeof(*ring));
  /* Parent input charge is conservatively retained after source Start. The
   * child admission separately charges its owned input. This is an admission
   * upper bound, not exact live allocation bytes or an OS RSS measurement. */
  size_t charge_limit=sizeof(KuTaskInstance_0)+sizeof(KuTaskInstance_1)+62u;
  CHECK(ku_task_driver_init(driver,sizeof(*driver),KU_TASK_DRIVER_ABI_VERSION,
      slots,capacity,ring,capacity,fixed+charge_limit)==KU_TASK_DRIVER_OK);
  uint8_t* text=(uint8_t*)malloc(31u); CHECK(text); memcpy(text,"payload",7u);
  KuString input={text,7u,31u,KU_STRING_OWNED};
  FixtureLedger before=fixture_ledger();
  KuTaskValueV1 parent={0};
  CHECK(ku_task_0_start_value(driver,&input,&parent)==KU_TASK_DRIVER_OK);
  CHECK(parent.tag==KU_TASK_VALUE_LIVE && !input.ptr);
  uint64_t deadline=ku_task_driver_now_ms()+2000u;
  CHECK(ku_task_driver_wait_result(&parent.ticket,&parent.owner.lease,deadline)==KU_TASK_DRIVER_WAIT_READY);
  CHECK(ku_task_driver_wait_idle(driver,deadline)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot={0};
  CHECK(ku_task_driver_snapshot(driver,&snapshot)==KU_TASK_DRIVER_OK);
  CHECK(!snapshot.fault && snapshot.resident==1u && !snapshot.building && !snapshot.running && !snapshot.queued);
  FixtureLedger ready=fixture_ledger();
  CHECK(ready.calls==before.calls+(mode ? 1u : 2u));
  CHECK(ku_task_control_atomic_load(&fixture_calloc_attempts)==(mode==1 ? 0u : 1u));
  CHECK(fixture_charge.observations==(mode ? 0u : 2u));
  CHECK(snapshot.reserved_bytes==sizeof(KuTaskInstance_0)+(mode ? 31u : 62u));
  CHECK(ready.allocations==(mode ? 4u : 5u));
  CHECK(ready.bytes==fixed+sizeof(KuTaskInstance_0)+(mode ? 0u : 31u));
  KuTaskAdapterOutcomeV1 output={0};
  CHECK(ku_task_value_take(&parent,NULL,&output)==KU_TASK_CONTROL_OK);
  CHECK(output.result_kind==4u && output.exit_class==KU_TASK_EXIT_USER_RESULT);
  CHECK(!output.has_cleanup_deadline && !output.cleanup_deadline);
  if (!mode) {
    CHECK(output.value.string.ok && output.value.string.value.ptr==text && output.value.string.value.capacity==31u);
    fixture_text(output.value.string.value,"payload");
  } else {
    CHECK(!output.value.string.ok && !output.value.string.value.ptr);
    fixture_text(output.value.string.error.domain,"task");
    fixture_text(output.value.string.error.code,mode==1 ? "too_many_tasks" : "out_of_memory");
    CHECK(output.value.string.error.domain.storage==KU_STRING_STATIC);
    CHECK(output.value.string.error.code.storage==KU_STRING_STATIC);
    CHECK(output.value.string.error.message.storage==KU_STRING_STATIC);
  }
  CHECK(fixture_ledger().calls==ready.calls);
  ku_task_outcome_drop(&output);
  CHECK(fixture_ledger().allocations==4u && fixture_ledger().bytes==fixed+sizeof(KuTaskInstance_0));
  CHECK(ku_task_value_drop(&parent,deadline)==KU_TASK_DRIVER_OK);
  CHECK(!parent.tag && !parent.owner.lease.control);
  CHECK(ku_task_driver_shutdown(driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_snapshot(driver,&snapshot)==KU_TASK_DRIVER_OK);
  CHECK(!snapshot.fault && !snapshot.resident && !snapshot.reserved_bytes && !snapshot.building && !snapshot.queued && !snapshot.running);
#if defined(_WIN32)
  CHECK(WaitForSingleObject(driver->thread,2000)==WAIT_OBJECT_0);
#endif
  CHECK(ku_task_driver_destroy(driver)==KU_TASK_DRIVER_OK);
  free(ring); free(slots); free(driver);
  FixtureLedger final=fixture_ledger();
  CHECK(!final.allocations && !final.bytes && !final.overflow);
}
int main(void) {
  ku_task_control_atomic_init(&fixture_calloc_failure,0);
  ku_task_control_atomic_init(&fixture_calloc_attempts,0);
  fixture_rejection(0); fixture_rejection(1); fixture_rejection(2);
  puts("task-source-start-await-safety-ok");
  return 0;
}
"#;
