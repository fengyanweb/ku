//! Actual checked-expression failure through generated Task ownership/cleanup.
//! The host supplies legitimate Owned ABI values and a nonliteral integer;
//! this does not add dynamic allocation syntax to the source-language subset.
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
    ir::{task::TaskOp, task_lower},
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
    assert_eq!(
        source.matches(anchor).count(),
        1,
        "arithmetic safety hook: {anchor}"
    );
    source.replacen(anchor, replacement, 1)
}

#[test]
fn native_task_expression_failure_drops_partial_owned_arguments_and_drains_real_child() {
    let source = r#"
async fn Parent(value: str!, extra: str, number: int): int! {
    child = Child()
    result = (await Consume(value, extra, -number))?
    println("BAD")
    return ok(result)
}
async fn Child(): int! { return ok(7) }
async fn Consume(value: str!, extra: str, number: int): int! { return ok(number) }
async fn main(): null! { return ok(null) }
"#;
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let native = task_lower::lower_program(&ast).unwrap();
    for (id, name) in ["Parent", "Child", "Consume", "main"].iter().enumerate() {
        assert_eq!(native.tasks.functions[id].name, *name);
    }
    // Derive the actual compiler-owned argument snapshots rather than assuming
    // hard-coded slot numbering or replacing their move/drop implementation.
    let arguments: Vec<_> = native.tasks.functions[0]
        .states
        .iter()
        .flat_map(|state| &state.operations)
        .filter_map(|operation| match operation {
            TaskOp::Start {
                function,
                arguments,
                ..
            } if function.0 == 2 => Some(arguments.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(arguments.len(), 1);
    assert_eq!(arguments[0].len(), 3);
    assert_ne!(arguments[0][0].0, 0);
    assert_ne!(arguments[0][1].0, 1);
    let value_slot = arguments[0][0].0;
    let extra_slot = arguments[0][1].0;
    let generated = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
    for forbidden in ["run_source", "const SOURCE", "Task.new", "task.spawn"] {
        assert!(!generated.contains(forbidden));
    }
    let ledger = replace_once(
        LOCKED_ALLOCATIONS.to_owned(),
        "ku_perf_free(p);",
        "fixture_record_free(p); ku_perf_free(p);",
    );
    let instrumentation = format!(
        "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\nstatic void fixture_record_free(void*);\n{ledger}\nstatic void fixture_record_drive(void);\nstatic void fixture_child_cleanup(void*,uint32_t,void*);\n"
    );
    let generated = replace_once(
        generated,
        "typedef struct KuString {",
        &format!("{instrumentation}typedef struct KuString {{"),
    );
    // The wrapper invokes the real checked helper, then pauses after its actual
    // failure but before the generated frame publishes its runtime error.
    let generated = replace_once(
        generated,
        "static uint32_t ku_int_neg(int64_t value, int64_t* out) {",
        "static uint32_t ku_int_neg(int64_t,int64_t*);\nstatic uint32_t fixture_real_ku_int_neg(int64_t value, int64_t* out) {",
    );
    let generated = replace_once(
        generated,
        "static uint64_t ku_task_driver_now_ms(void) {",
        &format!("{CLOCK_HOOK}\nstatic uint64_t fixture_original_now(void) {{"),
    );
    let generated = replace_once(
        generated,
        "static uint32_t ku_task_frame_0_drive(KuTaskFrame_0* frame, const KuTaskFrameClockV1* clock, int cleanup) {",
        "static uint32_t ku_task_frame_0_drive(KuTaskFrame_0* frame, const KuTaskFrameClockV1* clock, int cleanup) {\n  if (!cleanup) fixture_record_drive();",
    );
    let generated = replace_once(
        generated,
        "static uint32_t ku_task_1_resume(void* raw) {",
        "static uint32_t ku_task_1_resume(void* raw) {\n  CHECK(0 && \"cancelled scope child must not execute its source body\");",
    );
    let generated = replace_once(
        generated,
        "static uint32_t ku_task_1_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {",
        "static uint32_t ku_task_1_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {\n  fixture_child_cleanup(raw,reason,budget);",
    );
    let generated = replace_once(
        generated,
        "static uint32_t ku_task_2_try_start_impl(KuTaskDriverV1* driver,\n    const KuTaskDriverStartChargeV1* source, KuResult_str* arg_0, KuString* arg_1, int64_t* arg_2, KuTaskHandle_2* output) {",
        "static uint32_t ku_task_2_try_start_impl(KuTaskDriverV1* driver,\n    const KuTaskDriverStartChargeV1* source, KuResult_str* arg_0, KuString* arg_1, int64_t* arg_2, KuTaskHandle_2* output) {\n  CHECK(0 && \"later Start must not execute after argument arithmetic fails\");",
    );
    let generated = replace_once(
        generated,
        "static uint32_t ku_task_driver_owner_drop_receipt(\n",
        "static uint32_t ku_task_driver_owner_drop_receipt(const KuTaskDriverTicketV1*,KuTaskControlOwnerV1*,uint64_t,KuTaskDriverCleanupReceiptV1*);\nstatic uint32_t fixture_real_ku_task_driver_owner_drop_receipt(\n",
    );
    let mut generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_unmodified_source_main(void) {",
    );
    generated.push_str(
        &C_MAIN
            .replace("@VALUE_SLOT@", &value_slot.to_string())
            .replace("@EXTRA_SLOT@", &extra_slot.to_string()),
    );
    let directory = TempDir::new("native-task-expression-safety");
    let path = directory.path().join("program.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute generated expression ownership safety"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("expression safety obeys the real process watchdog");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-expression-safety-ok\n"
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
static KuTestEvent fixture_entered,fixture_proceed;
static unsigned fixture_math_calls,fixture_drive_calls,fixture_cleanup_calls,fixture_receipt_calls;
static uint64_t fixture_child_deadline;
static KuTaskDriverTicketV1 fixture_child;
static KuTaskDriverCleanupReceiptV1 fixture_receipt;
static uintptr_t fixture_buffer[4];
static unsigned fixture_frees[4];
static size_t fixture_buffers;
static void fixture_record_free(void* pointer) {
  if (!pointer) return;
  uintptr_t identity=(uintptr_t)pointer; /* Convert only while allocation lives. */
  for (size_t i=0;i<fixture_buffers;i++) if (identity==fixture_buffer[i]) {
    CHECK(!fixture_frees[i]); fixture_frees[i]++;
  }
}
static void fixture_record_drive(void) { CHECK(!fixture_drive_calls++); }
static uint32_t ku_int_neg(int64_t value,int64_t* out) {
  CHECK(value==INT64_MIN && !fixture_math_calls++);
  uint32_t status=fixture_real_ku_int_neg(value,out);
  CHECK(status==KU_INT_OVERFLOW); /* Neither status nor out is fabricated. */
  CHECK(ku_test_event_set(&fixture_entered));
  CHECK(ku_test_event_wait(&fixture_proceed,2000));
  return status;
}
static void fixture_child_cleanup(void* raw,uint32_t reason,void* raw_budget) {
  KuTaskInstance_1* instance=(KuTaskInstance_1*)raw;
  KuTaskControlV1* budget=(KuTaskControlV1*)raw_budget;
  CHECK(!fixture_cleanup_calls++ && reason==KU_TASK_CONTROL_CANCELLED);
  CHECK(instance->ticket.driver==fixture_child.driver && instance->ticket.slot==fixture_child.slot
      && instance->ticket.generation==fixture_child.generation);
  fixture_child_deadline=ku_task_control_cleanup_deadline(budget);
}
static uint32_t ku_task_driver_owner_drop_receipt(const KuTaskDriverTicketV1* ticket,
    KuTaskControlOwnerV1* owner,uint64_t deadline,KuTaskDriverCleanupReceiptV1* receipt) {
  CHECK(!fixture_receipt_calls++ && ticket->driver==fixture_child.driver
      && ticket->slot==fixture_child.slot && ticket->generation==fixture_child.generation);
  uint32_t status=fixture_real_ku_task_driver_owner_drop_receipt(ticket,owner,deadline,receipt);
  CHECK(status==KU_TASK_DRIVER_OK && receipt->driver==ticket->driver
      && receipt->slot==ticket->slot && receipt->generation==ticket->generation);
  fixture_receipt=*receipt;
  return status;
}
static KuString fixture_owned(size_t capacity,const char* text) {
  size_t length=strlen(text); CHECK(length<=capacity && fixture_buffers<4u);
  uint8_t* buffer=(uint8_t*)malloc(capacity); CHECK(buffer); memcpy(buffer,text,length);
  fixture_buffer[fixture_buffers++]=(uintptr_t)buffer;
  KuString result={buffer,length,capacity,KU_STRING_OWNED}; return result;
}
static int fixture_empty_string(KuString value) {
  return !value.ptr && !value.len && !value.capacity && !value.storage;
}
static int fixture_empty_result(KuResult_str value) {
  return !value.ok && fixture_empty_string(value.value) && fixture_empty_string(value.error.domain)
      && fixture_empty_string(value.error.code) && fixture_empty_string(value.error.message);
}
static void fixture_same_string(KuString value,KuString expected) {
  CHECK(value.ptr==expected.ptr && value.len==expected.len && value.capacity==expected.capacity && value.storage==expected.storage);
}
static void fixture_same_result(KuResult_str value,KuResult_str expected) {
  CHECK(value.ok==expected.ok); fixture_same_string(value.value,expected.value);
  fixture_same_string(value.error.domain,expected.error.domain);
  fixture_same_string(value.error.code,expected.error.code);
  fixture_same_string(value.error.message,expected.error.message);
}
static KuTaskDriverSnapshotV1 fixture_idle(KuTaskDriverV1* driver) {
  CHECK(ku_task_driver_wait_idle(driver,ku_task_driver_now_ms()+2000u)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot={0};
  CHECK(ku_task_driver_snapshot(driver,&snapshot)==KU_TASK_DRIVER_OK);
  CHECK(!snapshot.fault && !snapshot.running && !snapshot.queued && !snapshot.building);
  CHECK(snapshot.worker_target==1u && snapshot.workers_created==1u);
  CHECK(snapshot.workers_waiting+snapshot.workers_exited==snapshot.workers_created);
  CHECK(!ku_task_driver_lock(driver));
  CHECK(!driver->clock_fault);
  size_t charge=0;
  for (size_t i=0;i<driver->capacity;i++) {
    CHECK(driver->slots[i].state!=KU_TASK_DRIVER_FAULTED);
    CHECK(driver->slots[i].charged_bytes<=SIZE_MAX-charge); charge+=driver->slots[i].charged_bytes;
  }
  CHECK(charge==driver->reserved_bytes);
  CHECK(!ku_task_driver_unlock(driver));
  return snapshot;
}
static void fixture_case(unsigned mode,int error) {
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes);
  fixture_math_calls=fixture_drive_calls=fixture_cleanup_calls=fixture_receipt_calls=0;
  fixture_buffers=0; memset(fixture_frees,0,sizeof(fixture_frees));
  fixture_child=(KuTaskDriverTicketV1){0}; fixture_receipt=(KuTaskDriverCleanupReceiptV1){0};
  fixture_child_deadline=0;
  CHECK(ku_test_event_init(&fixture_entered)); CHECK(ku_test_event_init(&fixture_proceed));
  ku_task_control_deadline_store(&fixture_clock,ku_test_real_now_ms()+100000u);
  uint64_t cleanup_now=ku_task_driver_now_ms();
  CHECK(cleanup_now!=UINT64_MAX && cleanup_now<UINT64_MAX-1000u);
  uint64_t deadline=cleanup_now+(mode ? 600u : 1000u);
  KuTaskDriverV1* driver=(KuTaskDriverV1*)calloc(1,sizeof(*driver));
  KuTaskDriverSlotV1* slots=(KuTaskDriverSlotV1*)calloc(2,sizeof(*slots));
  size_t* ring=(size_t*)calloc(2,sizeof(*ring)); CHECK(driver && slots && ring);
  size_t fixed=sizeof(*driver)+2u*(sizeof(*slots)+sizeof(*ring));
  size_t instances=sizeof(KuTaskInstance_0)+sizeof(KuTaskInstance_1);
  uint64_t startup_now=ku_task_driver_now_ms();
  CHECK(startup_now!=UINT64_MAX && startup_now<UINT64_MAX-1000u);
  uint64_t startup_deadline=startup_now+1000u;
  CHECK(ku_task_driver_init(driver,sizeof(*driver),6u,
      slots,2,ring,2,fixed+instances+48u,1u,startup_deadline)==KU_TASK_DRIVER_ABI_MISMATCH);
  CHECK(ku_task_frame_zero_bytes(driver,sizeof(*driver))
      && ku_task_frame_zero_bytes(slots,(2)*sizeof(*slots))
      && ku_task_frame_zero_bytes(ring,(2)*sizeof(*ring)));
  CHECK(ku_task_driver_init(driver,sizeof(*driver),KU_TASK_DRIVER_ABI_VERSION,
      slots,2,ring,2,fixed+instances+48u,1u,startup_deadline)==KU_TASK_DRIVER_OK);
  KuResult_str value={0};
  if (error) {
    value.error.domain=fixture_owned(7u,"domain");
    value.error.code=fixture_owned(11u,"code");
    value.error.message=fixture_owned(13u,"message");
  } else { value.ok=true; value.value=fixture_owned(31u,"payload"); }
  KuString extra=fixture_owned(17u,"extra");
  KuResult_str expected=value; KuString expected_extra=extra;
  int64_t number=INT64_MIN;
  KuTaskValueV1 root={0};
  CHECK(ku_task_0_start_value(driver,&value,&extra,&number,&root)==KU_TASK_DRIVER_OK);
  CHECK(fixture_empty_result(value) && fixture_empty_string(extra) && number==INT64_MIN);
  CHECK(ku_test_event_wait(&fixture_entered,2000));
  KuTaskInstance_0* parent=(KuTaskInstance_0*)root.owner.lease.control;
  /* The single worker is inside the real helper's post-check event barrier.
   * It cannot mutate these live frame/header fields until proceed is signaled. */
  CHECK(fixture_empty_result(parent->frame.s_0) && fixture_empty_string(parent->frame.s_1));
  CHECK(!(parent->frame.header.initialized&UINT64_C(3)));
  CHECK(parent->frame.header.initialized&(UINT64_C(1)<<@VALUE_SLOT@));
  CHECK(parent->frame.header.initialized&(UINT64_C(1)<<@EXTRA_SLOT@));
  fixture_same_result(parent->frame.s_@VALUE_SLOT@,expected);
  fixture_same_string(parent->frame.s_@EXTRA_SLOT@,expected_extra);
  CHECK(parent->frame.s_2==INT64_MIN && !parent->payload_initialized && !parent->drain_started);
  KuTaskControlLeaseV1 child_observer={0};
  CHECK(!ku_task_driver_lock(driver));
  CHECK(driver->running==1u && driver->queued==1u && driver->resident==2u);
  CHECK(driver->reserved_bytes==instances+48u && !driver->fault && !driver->clock_fault);
  for (size_t i=0;i<2;i++) if (slots[i].binding!=&parent->control) {
    CHECK(slots[i].binding && slots[i].binding->operations.resume==ku_task_1_resume);
    CHECK(!child_observer.control && ku_task_control_lease_retain(&slots[i].driver_lease,&child_observer)==KU_TASK_CONTROL_OK);
    fixture_child=((KuTaskInstance_1*)child_observer.control)->ticket;
    CHECK(slots[i].charged_bytes==sizeof(KuTaskInstance_1));
  }
  CHECK(child_observer.control && !ku_task_driver_unlock(driver));
  FixtureLedger before=fixture_ledger();
  CHECK(before.allocations==5u+fixture_buffers && before.bytes==fixed+instances+48u);
  uint32_t terminal=mode==2u ? KU_TASK_CONTROL_TIMED_OUT : KU_TASK_CONTROL_CANCELLED;
  if (mode) {
    CHECK(ku_task_driver_request_cancel(&root.ticket,&root.owner.lease,terminal,deadline)==KU_TASK_CONTROL_OK);
    uint32_t losing=mode==2u ? KU_TASK_CONTROL_CANCELLED : KU_TASK_CONTROL_TIMED_OUT;
    CHECK(ku_task_driver_request_cancel(&root.ticket,&root.owner.lease,losing,deadline+300u)==KU_TASK_CONTROL_OK);
    CHECK(ku_task_control_cleanup_deadline(&parent->control)==deadline);
    CHECK(ku_task_control_atomic_load(&parent->control.phase)==(mode==2u ? KU_TASK_CONTROL_REQUESTED_TIMEOUT : KU_TASK_CONTROL_REQUESTED_CANCEL));
  }
  CHECK(ku_test_event_set(&fixture_proceed));
  KuTaskDriverSnapshotV1 ready=fixture_idle(driver);
  CHECK(ready.resident==2u && ready.reserved_bytes==instances+48u);
  CHECK(fixture_math_calls==1u && fixture_drive_calls==1u && fixture_cleanup_calls==1u && fixture_receipt_calls==1u);
  CHECK(ku_task_driver_cleanup_receipt_read(&fixture_receipt)==KU_TASK_DRIVER_CLEANUP_ACK);
  CHECK(ku_task_control_atomic_load(&child_observer.control->phase)==KU_TASK_CONTROL_CANCELLED);
  CHECK(child_observer.control->frame_destroyed && !ku_task_control_atomic_load(&child_observer.control->lifecycle_pin));
  CHECK(fixture_child_deadline==deadline && ku_task_control_cleanup_deadline(child_observer.control)==deadline);
  CHECK(parent->control.frame_destroyed && !parent->frame_initialized && !parent->frame.header.initialized);
  CHECK(parent->frame.header.status==KU_TASK_FRAME_DESTROYED);
  CHECK(fixture_empty_result(parent->frame.s_@VALUE_SLOT@) && fixture_empty_string(parent->frame.s_@EXTRA_SLOT@));
  CHECK(parent->drain_started && parent->drain_deadline==deadline);
  FixtureLedger clean=fixture_ledger();
  CHECK(clean.allocations==5u && clean.bytes==fixed+instances && clean.calls==before.calls);
  for (size_t i=0;i<fixture_buffers;i++) CHECK(fixture_frees[i]==1u);
  CHECK(ku_task_control_atomic_load(&parent->control.phase)==(mode ? terminal : KU_TASK_CONTROL_FAILED));
  if (!mode) CHECK(parent->exit_class==KU_TASK_EXIT_RUNTIME_FAILURE && parent->has_cleanup_deadline && parent->cleanup_deadline==deadline);
  /* A real terminal notification must not replay the source frame, trap,
   * argument moves, receipt transfer, or user child body. */
  uint64_t polls=ready.polls;
  CHECK(ku_task_driver_wake(&root.ticket)==KU_TASK_DRIVER_OK);
  CHECK(fixture_idle(driver).polls==polls+1u);
  CHECK(fixture_math_calls==1u && fixture_drive_calls==1u && fixture_cleanup_calls==1u && fixture_receipt_calls==1u);
  KuTaskAdapterOutcomeV1 outcome={0};
  uint32_t taken=ku_task_value_take(&root,NULL,&outcome);
  if (mode) {
    CHECK(taken==terminal && ku_task_outcome_empty(&outcome,1u));
    CHECK(ku_task_control_atomic_load(&parent->control.phase)==terminal);
  } else {
    CHECK(taken==KU_TASK_CONTROL_OK && outcome.result_kind==1u && !outcome.value.integer.ok);
    CHECK(outcome.exit_class==KU_TASK_EXIT_RUNTIME_FAILURE && outcome.has_cleanup_deadline && outcome.cleanup_deadline==deadline);
    CHECK(!outcome.value.integer.error.domain.len && !outcome.value.integer.error.code.len);
    CHECK(outcome.value.integer.error.message.len==16u && outcome.value.integer.error.message.storage==KU_STRING_STATIC);
    CHECK(!memcmp(outcome.value.integer.error.message.ptr,"integer overflow",16u));
  }
  ku_task_outcome_drop(&outcome);
  CHECK(ku_task_value_drop(&root,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_control_lease_release(&child_observer)==KU_TASK_CONTROL_OK);
  CHECK(ku_task_driver_shutdown(driver,deadline)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 empty=fixture_idle(driver); CHECK(!empty.resident && !empty.reserved_bytes);
  CHECK(ku_task_driver_join(driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(driver->worker_target==1u && driver->workers_created==1u && driver->workers_exited==1u
      && driver->workers_joined==1u && driver->workers[0].joined && driver->workers[0].closed);
  CHECK(ku_task_driver_destroy(driver)==KU_TASK_DRIVER_OK);
  free(ring); free(slots); free(driver);
  for (size_t i=0;i<fixture_buffers;i++) CHECK(fixture_frees[i]==1u);
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes && !fixture_ledger().overflow);
  CHECK(ku_test_event_destroy(&fixture_entered)); CHECK(ku_test_event_destroy(&fixture_proceed));
}
int main(void) {
  ku_task_control_deadline_init(&fixture_clock);
  for (int error=0;error<2;error++) for (unsigned mode=0;mode<3u;mode++) fixture_case(mode,error);
  puts("task-expression-safety-ok"); return 0;
}
"#;
