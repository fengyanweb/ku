//! Real source composition: an unawaited child, early Result failure, and a
//! conditional second Start/Await. Owned inputs enter through the typed host
//! factory; this does not introduce source allocation or user scheduling APIs.
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
    assert_eq!(
        source.matches(anchor).count(),
        1,
        "conditional owner hook: {anchor}"
    );
    source.replacen(anchor, replacement, 1)
}

#[test]
fn native_task_short_circuit_owned_inputs_and_private_error_drain_execute_in_c() {
    let source = r#"
async fn Parent(gate: bool!, first: str, second: str): bool! {
    unawaited = Child(first)
    return ok((gate?) && (await Child(second))?)
}
async fn Child(value: str): bool! { println(value) return ok(true) }
async fn main(): null! { return ok(null) }
"#;
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let native = task_lower::lower_program(&ast).unwrap();
    let parent = &native.tasks.functions[0];
    assert_eq!(parent.name, "Parent");
    assert_eq!(native.tasks.functions[1].name, "Child");
    assert_eq!(parent.parameters.len(), 3);
    assert_eq!(
        parent
            .states
            .iter()
            .flat_map(|state| &state.operations)
            .filter(|op| matches!(op, ku::ir::task::TaskOp::Start { .. }))
            .count(),
        2
    );
    let task_mask = parent
        .slots
        .iter()
        .enumerate()
        .filter(|(_, slot)| matches!(slot.ty, ku::ir::task::TaskSlotType::Task { .. }))
        .fold(0u64, |mask, (index, _)| mask | (1u64 << index));
    assert_ne!(task_mask, 0);
    let mut generated = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
    assert!(!generated.contains("run_source") && !generated.contains("const SOURCE"));
    let ledger = replace_once(
        LOCKED_ALLOCATIONS.to_owned(),
        "ku_perf_free(p);",
        "fixture_record_free(p); ku_perf_free(p);",
    );
    let instrumentation = format!(
        "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\nstatic void fixture_record_free(void*);\n{ledger}\nstatic void* fixture_child_calloc(size_t,size_t);\nstatic uint32_t fixture_child_cleanup(void*);\nstatic void fixture_worker_exited(void);\n"
    );
    generated = replace_once(
        generated,
        "typedef struct KuString {",
        &format!("{instrumentation}typedef struct KuString {{"),
    );
    // These forwarding wrappers count real factory/admission work. They never
    // substitute a success, refusal, move, or publication implementation.
    for (name, prototype) in [
        ("ku_task_1_try_start_impl", "KuTaskDriverV1*,const KuTaskDriverStartChargeV1*,KuString*,KuTaskHandle_1*"),
        ("ku_task_driver_reserve_child", "KuTaskDriverV1*,const KuTaskDriverStartChargeV1*,size_t,KuTaskDriverTicketV1*"),
        ("ku_task_driver_commit_child", "const KuTaskDriverTicketV1*,KuTaskControlOwnerV1*,const KuTaskDriverStartChargeV1*,size_t"),
    ] {
        generated = replace_once(generated, &format!("static uint32_t {name}("),
            &format!("static uint32_t {name}({prototype});\nstatic uint32_t fixture_real_{name}("));
    }
    generated = replace_once(generated,
        "KuTaskInstance_1* instance = (KuTaskInstance_1*)calloc(1, sizeof(*instance));",
        "KuTaskInstance_1* instance = (KuTaskInstance_1*)fixture_child_calloc(1, sizeof(*instance));");
    generated = replace_once(
        generated,
        "static uint64_t ku_task_driver_now_ms(void) {",
        &format!("{CLOCK_HOOK}\nstatic uint64_t fixture_original_now(void) {{"),
    );
    generated = replace_once(generated,
        "static uint32_t ku_task_1_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {",
        "static uint32_t ku_task_1_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {\n  uint32_t held=fixture_child_cleanup(raw);\n  if (held!=UINT32_MAX) return held;");
    generated = replace_once(generated,
        "  /* No further driver or task storage access after this point. */",
        "  /* No further driver or task storage access after this point. */\n  fixture_worker_exited();");
    generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_unmodified_source_main(void) {",
    );
    generated.push_str(&C_MAIN.replace("@TASK_MASK@", &task_mask.to_string()));
    let directory = TempDir::new("native-task-conditional-owner");
    let path = directory.path().join("program.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute the generated conditional-owner fixture"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("conditional owner process has a hard watchdog");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "first\nsecond\ntask-conditional-owner-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const CLOCK_HOOK: &str = r#"
static KuTaskControlDeadlineV1 fixture_clock;
static uint64_t fixture_original_now(void);
static uint64_t ku_task_driver_now_ms(void) {
  uint64_t now=ku_task_control_deadline_load(&fixture_clock);
  return now ? now : fixture_original_now();
}
"#;

const C_MAIN: &str = r#"
static int fixture_empty_result(KuResult_bool value) {
  return !value.ok && !value.value
      && ku_task_outcome_empty_string(value.error.domain)
      && ku_task_outcome_empty_string(value.error.code)
      && ku_task_outcome_empty_string(value.error.message);
}
enum { GATE_ERR=0, GATE_FALSE=1, GATE_TRUE=2, GATE_REJECT=3, GATE_CANCEL=4 };
static unsigned fixture_mode,fixture_starts,fixture_reserves,fixture_commits,fixture_callocs;
static uintptr_t fixture_ids[5];
static unsigned fixture_frees[5];
static const size_t fixture_caps[5]={19u,23u,29u,31u,37u};
static KuAtomicRefcount fixture_hold;
static KuTestEvent fixture_exit;
typedef struct FixtureDriver {
  KuTaskDriverV1* driver;
  KuTaskDriverSlotV1* slots;
  size_t* ring;
} FixtureDriver;
static void fixture_record_free(void* p) {
  /* Called under the existing allocation mutex while p is still live. Store
   * only integer identities; never read a pointer value after its free. */
  if (!p) return;
  uintptr_t identity=(uintptr_t)p;
  for (size_t i=0;i<5u;i++) if (fixture_ids[i] && fixture_ids[i]==identity) {
    CHECK(!fixture_frees[i]); fixture_frees[i]++;
  }
}
static void fixture_worker_exited(void) { CHECK(ku_test_event_set(&fixture_exit)); }
static void* fixture_child_calloc(size_t count,size_t bytes) {
  CHECK(count==1u && bytes==sizeof(KuTaskInstance_1));
  fixture_callocs++; return calloc(count,bytes);
}
static uint32_t ku_task_driver_reserve_child(KuTaskDriverV1* driver,
    const KuTaskDriverStartChargeV1* source,size_t bytes,KuTaskDriverTicketV1* ticket) {
  fixture_reserves++; CHECK(source && bytes==sizeof(KuTaskInstance_1));
  uint32_t status=fixture_real_ku_task_driver_reserve_child(driver,source,bytes,ticket);
  CHECK(status==((fixture_mode==GATE_REJECT && fixture_reserves==2u)
      ? KU_TASK_DRIVER_LIMIT : KU_TASK_DRIVER_OK));
  return status;
}
static uint32_t ku_task_driver_commit_child(const KuTaskDriverTicketV1* ticket,
    KuTaskControlOwnerV1* owner,const KuTaskDriverStartChargeV1* source,size_t bytes) {
  fixture_commits++;
  uint32_t status=fixture_real_ku_task_driver_commit_child(ticket,owner,source,bytes);
  CHECK(status==KU_TASK_DRIVER_OK); return status;
}
static uint32_t ku_task_1_try_start_impl(KuTaskDriverV1* driver,
    const KuTaskDriverStartChargeV1* source,KuString* input,KuTaskHandle_1* output) {
  CHECK(source && fixture_starts<2u);
  unsigned index=fixture_starts++;
  CHECK((uintptr_t)input->ptr==fixture_ids[index] && input->capacity==fixture_caps[index]);
  uint32_t status=fixture_real_ku_task_1_try_start_impl(driver,source,input,output);
  if (fixture_mode==GATE_REJECT && index==1u) {
    CHECK(status==KU_TASK_DRIVER_LIMIT && (uintptr_t)input->ptr==fixture_ids[index]);
    CHECK(!output->owner.lease.control && !output->ticket.driver);
  } else CHECK(status==KU_TASK_DRIVER_OK && !input->ptr && output->owner.lease.control);
  return status;
}
static uint32_t fixture_child_cleanup(void* raw) {
  if (!ku_task_control_atomic_load(&fixture_hold)) return UINT32_MAX;
  KuTaskInstance_1* child=(KuTaskInstance_1*)raw;
  /* Defer only progress: no fake frame cleanup, receipt ACK or terminal value. */
  CHECK(ku_task_driver_set_intent(&child->ticket,KU_TASK_DRIVER_WAIT)==KU_TASK_DRIVER_OK);
  return KU_TASK_CONTROL_PENDING;
}
static KuString fixture_string(size_t index,const char* text) {
  KuString out={0}; out.len=strlen(text); out.capacity=fixture_caps[index];
  CHECK(out.len<=out.capacity); out.ptr=(uint8_t*)malloc(out.capacity); CHECK(out.ptr);
  memcpy(out.ptr,text,out.len); out.storage=KU_STRING_OWNED;
  fixture_ids[index]=(uintptr_t)out.ptr; return out;
}
static KuTaskDriverSnapshotV1 fixture_snapshot(FixtureDriver* runtime) {
  KuTaskDriverSnapshotV1 snapshot={0};
  CHECK(ku_task_driver_snapshot(runtime->driver,&snapshot)==KU_TASK_DRIVER_OK);
  CHECK(snapshot.worker_target==1u && snapshot.workers_created==1u);
  CHECK(!snapshot.fault && snapshot.running<=1u && snapshot.resident<=3u);
  return snapshot;
}
static void fixture_idle(FixtureDriver* runtime) {
  /* This is a condition wait, not polling. The frozen clock never changes D;
   * the existing real 20-second process watchdog bounds lost progress. */
  CHECK(ku_task_driver_wait_idle(runtime->driver,ku_task_driver_now_ms()+2000u)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot=fixture_snapshot(runtime);
  CHECK(!snapshot.running && !snapshot.queued && (snapshot.workers_waiting + snapshot.workers_exited == snapshot.workers_created));
}
static void fixture_init(FixtureDriver* runtime,size_t capacity,size_t input_bytes) {
  memset(runtime,0,sizeof(*runtime));
  runtime->driver=(KuTaskDriverV1*)calloc(1,sizeof(*runtime->driver));
  runtime->slots=(KuTaskDriverSlotV1*)calloc(capacity,sizeof(*runtime->slots));
  runtime->ring=(size_t*)calloc(capacity,sizeof(*runtime->ring));
  CHECK(runtime->driver && runtime->slots && runtime->ring);
  size_t fixed=sizeof(*runtime->driver)+capacity*(sizeof(*runtime->slots)+sizeof(*runtime->ring));
  /* Exact conservative admission accounting, not a claim about allocator RSS:
   * each hosted Start adds I, transferring the existing A charge once. */
  size_t tasks=sizeof(KuTaskInstance_0)+(capacity-1u)*sizeof(KuTaskInstance_1)+input_bytes;
  uint64_t startup_now=ku_task_driver_now_ms();
  CHECK(startup_now<=UINT64_MAX-1000u);
  uint64_t startup_deadline=startup_now+1000u;
  CHECK(startup_deadline!=UINT64_MAX);
  CHECK(KU_TASK_DRIVER_ABI_VERSION==7u);
  CHECK(ku_task_driver_init(runtime->driver,sizeof(*runtime->driver),KU_TASK_DRIVER_ABI_VERSION,
      runtime->slots,capacity,runtime->ring,capacity,fixed+tasks,1u,startup_deadline)==KU_TASK_DRIVER_OK);
  fixture_idle(runtime);
}
static void fixture_check_held(FixtureDriver* runtime,const KuTaskDriverCleanupReceiptV1* receipt,
    const KuTaskControlLeaseV1* child,uint64_t deadline) {
  CHECK(ku_task_driver_cleanup_receipt_read(receipt)==KU_TASK_DRIVER_PENDING);
  CHECK(!ku_task_driver_lock(runtime->driver));
  KuTaskDriverSlotV1* slot=&runtime->slots[receipt->slot];
  CHECK(slot->generation==receipt->generation && slot->binding==child->control);
  CHECK(slot->owner_location==KU_TASK_DRIVER_OWNER_RELEASED && !slot->cleanup_acked_generation);
  CHECK(slot->cancel_deadline==deadline);
  CHECK(ku_task_control_atomic_load(&child->control->phase)==KU_TASK_CONTROL_REQUESTED_CANCEL);
  CHECK(ku_task_control_cleanup_deadline(child->control)==deadline);
  CHECK(!ku_task_driver_unlock(runtime->driver));
}
static void fixture_error_identity(const KuError* error) {
  const KuString* fields[3]={&error->domain,&error->code,&error->message};
  const char* texts[3]={"gate-domain","gate-code","gate-message"};
  for (size_t i=0;i<3u;i++) {
    CHECK((uintptr_t)fields[i]->ptr==fixture_ids[i+2u]);
    CHECK(fields[i]->capacity==fixture_caps[i+2u] && fields[i]->storage==KU_STRING_OWNED);
    CHECK(fields[i]->len==strlen(texts[i]) && !memcmp(fields[i]->ptr,texts[i],fields[i]->len));
    CHECK(!fixture_frees[i+2u]);
  }
}
static void fixture_finish(FixtureDriver* runtime,uint64_t deadline) {
  CHECK(ku_task_driver_shutdown(runtime->driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_test_event_wait(&fixture_exit,2000));
  KuTaskDriverSnapshotV1 exited=fixture_snapshot(runtime);
  CHECK(exited.workers_exited==1u && !exited.workers_waiting && !exited.workers_joined);
  CHECK(!exited.resident && !exited.building && !exited.running && !exited.queued && !exited.reserved_bytes);
#if !defined(_WIN32)
  alarm(2); /* Bound the existing POSIX OS-return tail, not a new cleanup D. */
#endif
  CHECK(ku_task_driver_join(runtime->driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(runtime->driver->workers_joined==1u && runtime->driver->workers[0].joined && runtime->driver->workers[0].closed);
  CHECK(ku_task_driver_destroy(runtime->driver)==KU_TASK_DRIVER_OK);
#if !defined(_WIN32)
  alarm(0);
#endif
  free(runtime->ring); free(runtime->slots); free(runtime->driver);
  CHECK(ku_test_event_destroy(&fixture_exit));
  FixtureLedger ledger=fixture_ledger();
  CHECK(!ledger.allocations && !ledger.bytes && !ledger.overflow);
}
static void fixture_case(unsigned mode) {
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes);
  fixture_mode=mode; fixture_starts=fixture_reserves=fixture_commits=fixture_callocs=0;
  memset(fixture_ids,0,sizeof(fixture_ids)); memset(fixture_frees,0,sizeof(fixture_frees));
  ku_task_control_atomic_store(&fixture_hold,mode!=GATE_TRUE);
  ku_task_control_deadline_store(&fixture_clock,ku_test_real_now_ms()+100000u);
  CHECK(ku_test_event_init(&fixture_exit));
  int error_input=mode==GATE_ERR || mode==GATE_CANCEL;
  size_t inputs=fixture_caps[0]+fixture_caps[1];
  if (error_input) inputs+=fixture_caps[2]+fixture_caps[3]+fixture_caps[4];
  FixtureDriver runtime; fixture_init(&runtime,mode==GATE_TRUE ? 3u : 2u,inputs);
  KuString first=fixture_string(0,"first"),second=fixture_string(1,"second");
  KuResult_bool gate={0};
  if (error_input) {
    gate.error.domain=fixture_string(2,"gate-domain");
    gate.error.code=fixture_string(3,"gate-code");
    gate.error.message=fixture_string(4,"gate-message");
  } else { gate.ok=true; gate.value=mode!=GATE_FALSE; }
  KuTaskValueV1 parent={0};
  CHECK(ku_task_0_start_value(runtime.driver,&gate,&first,&second,&parent)==KU_TASK_DRIVER_OK);
  CHECK(parent.tag==KU_TASK_VALUE_LIVE && !first.ptr && !second.ptr);
  CHECK(!gate.ok && !gate.error.domain.ptr && !gate.error.code.ptr && !gate.error.message.ptr);
  KuTaskControlLeaseV1 observer={0},child_observer={0};
  CHECK(ku_task_control_lease_retain(&parent.owner.lease,&observer)==KU_TASK_CONTROL_OK);
  fixture_idle(&runtime);
  KuTaskInstance_0* instance=(KuTaskInstance_0*)observer.control;
  uint64_t original=ku_task_driver_now_ms()+1000u,deadline=original;
  KuTaskDriverCleanupReceiptV1 receipt={0}; KuTaskDriverTicketV1 child_ticket={0};
  if (mode!=GATE_TRUE) {
    /* No worker can mutate these fields after the real quiescent wait.
     * External owner/observer leases protect the inspected allocations. */
    CHECK(instance->payload_initialized && instance->frame.header.status==KU_TASK_FRAME_READY);
    CHECK(!(instance->frame.header.initialized & UINT64_C(@TASK_MASK@)));
    CHECK(instance->drain_started && instance->drain_deadline_published);
    CHECK(instance->drain_deadline==original && instance->drain_published_deadline==original);
    CHECK(ku_task_control_atomic_load(&observer.control->phase)==KU_TASK_CONTROL_LIVE);
    CHECK(ku_task_control_atomic_load(&observer.control->payload)==KU_TASK_CONTROL_PAYLOAD_EMPTY);
    KuTaskAdapterOutcomeV1 early={0};
    CHECK(ku_task_value_take(&parent,NULL,&early)==KU_TASK_CONTROL_PENDING);
    CHECK(ku_task_outcome_empty(&early,2u) && parent.tag==KU_TASK_VALUE_LIVE);
    /* The public driver wrapper notifies after a take attempt, including
     * Pending. Drain that real notification before inspecting the frame or
     * establishing a no-new-event poll-count baseline. */
    fixture_idle(&runtime);
    CHECK(parent.owner.lease.control==observer.control && instance->payload_initialized);
    if (error_input) { CHECK(!instance->payload.ok); fixture_error_identity(&instance->payload.error); }
    if (mode==GATE_FALSE) CHECK(instance->payload.ok && !instance->payload.value);
    CHECK(!fixture_frees[0] && fixture_frees[1]==1u);
    KuTaskDriverWaitSnapshotV1 wait={0};
    CHECK(ku_task_driver_wait_read(&instance->wait,&wait)==KU_TASK_DRIVER_OK);
    CHECK(wait.kind==KU_TASK_DRIVER_WAIT_KIND_CLEANUP_ACK && wait.state==KU_TASK_DRIVER_WAIT_ARMED);
    size_t count=0; CHECK(!ku_task_driver_lock(runtime.driver));
    for (size_t i=0;i<sizeof(instance->receipts)/sizeof(instance->receipts[0]);i++) {
      if (!(instance->receipt_mask & (UINT64_C(1)<<i))) continue;
      CHECK(!count++); receipt=instance->receipts[i];
      KuTaskDriverSlotV1* slot=&runtime.slots[receipt.slot];
      CHECK(ku_task_control_lease_retain(&slot->driver_lease,&child_observer)==KU_TASK_CONTROL_OK);
      child_ticket=((KuTaskInstance_1*)child_observer.control)->ticket;
    }
    CHECK(count==1u); CHECK(!ku_task_driver_unlock(runtime.driver));
    fixture_check_held(&runtime,&receipt,&child_observer,deadline);
    if (mode==GATE_CANCEL) {
      uint64_t epoch=instance->wait.epoch; deadline=original-300u;
      CHECK(ku_task_driver_request_cancel(&parent.ticket,&observer,KU_TASK_CONTROL_CANCELLED,deadline)==KU_TASK_CONTROL_OK);
      fixture_idle(&runtime);
      /* The finished Exit frame is empty but stays READY until the real child
       * ACK lets R2 destroy it. Its discarded private Err has no owner left. */
      CHECK(!instance->payload_initialized && instance->frame_initialized && instance->values_cleaned);
      CHECK(!instance->control.frame_destroyed && instance->frame.header.status==KU_TASK_FRAME_READY);
      CHECK(!instance->frame.header.initialized && !instance->frame.header.result_initialized);
      CHECK(fixture_empty_result(instance->payload) && fixture_empty_result(instance->frame.result));
      for (size_t i=2;i<5u;i++) CHECK(fixture_frees[i]==1u);
      CHECK(!fixture_frees[0] && fixture_frees[1]==1u);
      fixture_check_held(&runtime,&receipt,&child_observer,deadline);
      CHECK(instance->drain_deadline==deadline && instance->drain_published_deadline==deadline);
      CHECK(instance->wait.epoch==epoch);
      CHECK(ku_task_driver_request_cancel(&parent.ticket,&observer,KU_TASK_CONTROL_TIMED_OUT,original+1000u)==KU_TASK_CONTROL_OK);
      fixture_idle(&runtime); fixture_check_held(&runtime,&receipt,&child_observer,deadline);
      CHECK(ku_task_control_atomic_load(&observer.control->phase)==KU_TASK_CONTROL_REQUESTED_CANCEL);
      CHECK(ku_task_control_cleanup_deadline(observer.control)==deadline && instance->drain_deadline==deadline);
    }
    uint64_t polls=fixture_snapshot(&runtime).polls;
    for (unsigned i=0;i<3u;i++) { fixture_idle(&runtime); CHECK(fixture_snapshot(&runtime).polls==polls); }
    ku_task_control_atomic_store(&fixture_hold,0);
    CHECK(ku_task_driver_wake(&child_ticket)==KU_TASK_DRIVER_OK);
  }
  CHECK(ku_task_driver_wait_result(&parent.ticket,&parent.owner.lease,original+1000u)==KU_TASK_DRIVER_WAIT_READY);
  fixture_idle(&runtime);
  unsigned starts=(mode==GATE_TRUE || mode==GATE_REJECT) ? 2u : 1u;
  unsigned commits=mode==GATE_TRUE ? 2u : 1u;
  CHECK(fixture_starts==starts && fixture_reserves==starts);
  CHECK(fixture_commits==commits && fixture_callocs==commits);
  CHECK(fixture_frees[0]==1u && fixture_frees[1]==1u);
  if (mode!=GATE_TRUE) {
    CHECK(ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_CLEANUP_ACK);
    CHECK(ku_task_control_atomic_load(&child_observer.control->phase)==KU_TASK_CONTROL_CANCELLED);
    CHECK(ku_task_control_cleanup_deadline(child_observer.control)==deadline);
    CHECK(child_observer.control->frame_destroyed && !ku_task_control_atomic_load(&child_observer.control->lifecycle_pin));
  }
  CHECK(instance->control.frame_destroyed && !instance->frame_initialized && instance->values_cleaned);
  CHECK(instance->frame.header.status==KU_TASK_FRAME_DESTROYED);
  CHECK(!instance->frame.header.initialized && !instance->frame.header.result_initialized);
  CHECK(fixture_empty_result(instance->frame.result));
  CHECK(!ku_task_control_atomic_load(&instance->control.lifecycle_pin));
  if (mode==GATE_ERR) {
    CHECK(ku_task_control_atomic_load(&observer.control->phase)==KU_TASK_CONTROL_FAILED);
    /* A private Err is not a winner, but the real post-ACK FAILED publication
     * is: this late legal cancellation cannot replace or drop its payload. */
    CHECK(ku_task_driver_request_cancel(&parent.ticket,&observer,KU_TASK_CONTROL_CANCELLED,deadline)==KU_TASK_CONTROL_FAILED);
    fixture_idle(&runtime); fixture_error_identity(&instance->payload.error);
  }
  KuTaskAdapterOutcomeV1 outcome={0};
  uint32_t status=ku_task_value_take(&parent,NULL,&outcome);
  if (mode==GATE_CANCEL) {
    CHECK(status==KU_TASK_CONTROL_CANCELLED && ku_task_outcome_empty(&outcome,2u));
    CHECK(ku_task_control_atomic_load(&observer.control->phase)==KU_TASK_CONTROL_CANCELLED);
  } else {
    CHECK(status==KU_TASK_CONTROL_OK && outcome.result_kind==2u && outcome.exit_class==KU_TASK_EXIT_USER_RESULT);
    CHECK(!outcome.has_cleanup_deadline && !outcome.cleanup_deadline);
    if (mode==GATE_ERR) { CHECK(!outcome.value.boolean.ok); fixture_error_identity(&outcome.value.boolean.error); }
    else if (mode==GATE_REJECT) {
      KuError* error=&outcome.value.boolean.error;
      CHECK(!outcome.value.boolean.ok);
      CHECK(error->domain.len==4u && !memcmp(error->domain.ptr,"task",4u));
      CHECK(error->code.len==14u && !memcmp(error->code.ptr,"too_many_tasks",14u));
    } else CHECK(outcome.value.boolean.ok && outcome.value.boolean.value==(mode==GATE_TRUE));
  }
  fixture_idle(&runtime); /* Complete the real take wrapper's notification. */
  CHECK(!instance->payload_initialized && fixture_empty_result(instance->payload));
  ku_task_outcome_drop(&outcome);
  CHECK(ku_task_outcome_empty(&outcome,2u));
  CHECK(ku_task_value_drop(&parent,deadline)==KU_TASK_DRIVER_OK);
  CHECK(!parent.tag && !parent.owner.lease.control);
  CHECK(ku_task_control_lease_release(&observer)==KU_TASK_CONTROL_OK);
  if (child_observer.control) CHECK(ku_task_control_lease_release(&child_observer)==KU_TASK_CONTROL_OK);
  fixture_idle(&runtime);
  KuTaskDriverSnapshotV1 done=fixture_snapshot(&runtime);
  CHECK(!done.resident && !done.reserved_bytes && !done.building && !done.running && !done.queued);
  for (size_t i=0;i<5u;i++) CHECK(fixture_frees[i]==(fixture_ids[i] ? 1u : 0u));
  fixture_finish(&runtime,deadline);
}
int main(void) {
  for (unsigned mode=0;mode<5u;mode++) fixture_case(mode);
  puts("task-conditional-owner-ok"); return 0;
}
"#;
