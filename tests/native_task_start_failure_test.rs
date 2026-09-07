//! Real hosted factory rollback and Start/cancellation publication boundaries.
//! Faults reject genuine ABI/driver calls before publication; no hook returns
//! failure after a successful commit or replaces ownership/cleanup operations.
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
        "Start failure hook: {anchor}"
    );
    source.replacen(anchor, replacement, 1)
}

#[test]
fn native_task_start_real_factory_failures_and_cancel_closing_boundaries_execute_in_c() {
    let source = r#"
async fn Parent(value: str!): str! { child = Child(value) return await child }
async fn Child(value: str!): str! { return value }
async fn main(): null! { return ok(null) }
"#;
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let native = task_lower::lower_program(&ast).unwrap();
    assert_eq!(native.tasks.functions[0].name, "Parent");
    assert_eq!(native.tasks.functions[1].name, "Child");
    assert_eq!(native.tasks.functions[1].parameters[0].0, 0);
    let generated = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
    assert!(!generated.contains("run_source") && !generated.contains("const SOURCE"));
    let instrumentation = format!(
        "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\n{LOCKED_ALLOCATIONS}\nstatic void fixture_child_cleanup_observe(void*,uint32_t,void*);\nstatic void fixture_child_dispose_observe(void*);\nstatic void fixture_child_resume_observe(void*);\nstatic void fixture_worker_exit(void);\n"
    );
    let mut generated = replace_once(
        generated,
        "typedef struct KuString {",
        &format!("{instrumentation}typedef struct KuString {{"),
    );
    for (name, prototype) in [
        (
            "ku_task_control_init",
            "KuTaskControlV1*,size_t,uint32_t,const KuTaskControlOpsV1*,void*,KuTaskControlOwnerV1*",
        ),
        ("ku_task_control_poll", "const KuTaskControlLeaseV1*"),
        ("ku_task_control_owner_drop", "KuTaskControlOwnerV1*,uint64_t"),
        ("ku_task_control_finish_poll", "KuTaskControlV1*,uint32_t"),
        (
            "ku_task_frame_1_init",
            "void*,size_t,uint32_t,KuResult_str*",
        ),
        (
            "ku_task_driver_reserve_child",
            "KuTaskDriverV1*,const KuTaskDriverStartChargeV1*,size_t,KuTaskDriverTicketV1*",
        ),
        (
            "ku_task_driver_commit_child",
            "const KuTaskDriverTicketV1*,KuTaskControlOwnerV1*,const KuTaskDriverStartChargeV1*,size_t",
        ),
        (
            "ku_task_1_try_start_impl",
            "KuTaskDriverV1*,const KuTaskDriverStartChargeV1*,KuResult_str*,KuTaskHandle_1*",
        ),
    ] {
        generated = replace_once(
            generated,
            &format!("static uint32_t {name}("),
            &format!(
                "static uint32_t {name}({prototype});\nstatic uint32_t fixture_real_{name}("
            ),
        );
    }
    generated = replace_once(
        generated,
        "static uint64_t ku_task_driver_now_ms(void) {",
        &format!("{CLOCK_HOOK}\nstatic uint64_t fixture_original_now(void) {{"),
    );
    generated = replace_once(
        generated,
        "static uint32_t ku_task_1_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {",
        "static uint32_t ku_task_1_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {\n  fixture_child_cleanup_observe(raw,reason,budget);",
    );
    generated = replace_once(
        generated,
        "static uint32_t ku_task_1_resume(void* raw) {",
        "static uint32_t ku_task_1_resume(void* raw) {\n  fixture_child_resume_observe(raw);",
    );
    generated = replace_once(
        generated,
        "static void ku_task_1_dispose(KuTaskControlV1* control, void* raw) {",
        "static void ku_task_1_dispose(KuTaskControlV1* control, void* raw) {\n  fixture_child_dispose_observe(raw);",
    );
    generated = replace_once(
        generated,
        "  /* No further driver or task storage access after this point. */",
        "  /* No further driver or task storage access after this point. */\n  fixture_worker_exit();",
    );
    generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_unmodified_source_main(void) {",
    );
    generated.push_str(C_MAIN);
    let directory = TempDir::new("native-task-start-failure");
    let path = directory.path().join("program.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute real hosted Start rollback/cancellation tests"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("hosted Start failure process has a hard watchdog");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-start-failures-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const CLOCK_HOOK: &str = r#"
static KuAtomicRefcount fixture_bad_clock;
static uint64_t fixture_original_now(void);
static uint64_t ku_task_driver_now_ms(void) {
  return ku_task_control_atomic_load(&fixture_bad_clock) ? UINT64_MAX : fixture_original_now();
}
"#;

const C_MAIN: &str = r#"
enum { FIXTURE_NORMAL=0, FIXTURE_CONTROL_ABI=1, FIXTURE_FRAME_ABI=2,
       FIXTURE_STALE_COMMIT=3, FIXTURE_FIRST_RETAIN=4, FIXTURE_SECOND_RETAIN=5 };
enum { FIXTURE_RESERVE=1, FIXTURE_BEFORE_COMMIT=2, FIXTURE_AFTER_COMMIT=3,
       FIXTURE_AFTER_ROLLBACK=4 };
enum { FIXTURE_CANCEL=0, FIXTURE_TIMEOUT=1, FIXTURE_CLOSING=2, FIXTURE_CLOCK=3 };
static unsigned fixture_fault,fixture_pause,fixture_factory_calls,fixture_reserves;
static unsigned fixture_commits,fixture_private_polls,fixture_private_drops;
static unsigned fixture_child_disposes,fixture_child_resumes,fixture_cleanups;
static uint32_t fixture_lowlevel_status,fixture_cleanup_reason;
static uint64_t fixture_cleanup_deadline;
static size_t fixture_input_buffers,fixture_fixed;
static KuResult_str* fixture_argument;
static KuTaskDriverStartChargeV1 fixture_source;
static KuTaskDriverTicketV1 fixture_stale,fixture_child_ticket;
static KuTaskControlLeaseV1 fixture_child_observer;
/* Real, bounded leases, not forged refcount fields and not runtime allocations. */
static KuTaskControlLeaseV1 fixture_quota[KU_TASK_CONTROL_MAX_REFERENCES];
static KuTestEvent fixture_entered,fixture_proceed,fixture_parent_terminal,fixture_exited;
static void fixture_barrier(unsigned point) {
  if (fixture_pause != point) return;
  CHECK(ku_test_event_set(&fixture_entered));
  CHECK(ku_test_event_wait(&fixture_proceed,2000));
}
static void fixture_worker_exit(void) { CHECK(ku_test_event_set(&fixture_exited)); }
static void fixture_sum_locked(KuTaskDriverV1* driver) {
  size_t sum=0,count=0;
  for (size_t i=0;i<driver->capacity;i++) {
    KuTaskDriverSlotV1* slot=&driver->slots[i];
    if (slot->state==KU_TASK_DRIVER_FREE) { CHECK(!slot->charged_bytes); continue; }
    CHECK(slot->charged_bytes<=SIZE_MAX-sum); sum+=slot->charged_bytes; count++;
  }
  CHECK(sum==driver->reserved_bytes && count==driver->resident);
}
static int fixture_empty_string(KuString value) {
  return !value.ptr && !value.len && !value.capacity && !value.storage;
}
static int fixture_empty_result(KuResult_str value) {
  return !value.ok && fixture_empty_string(value.value)
      && fixture_empty_string(value.error.domain) && fixture_empty_string(value.error.code)
      && fixture_empty_string(value.error.message);
}
static void fixture_same_string(KuString actual,KuString expected) {
  CHECK(actual.ptr==expected.ptr && actual.len==expected.len
      && actual.capacity==expected.capacity && actual.storage==expected.storage);
  if (actual.len) CHECK(!memcmp(actual.ptr,expected.ptr,actual.len));
}
static void fixture_same_result(KuResult_str actual,KuResult_str expected) {
  CHECK(actual.ok==expected.ok);
  fixture_same_string(actual.value,expected.value);
  fixture_same_string(actual.error.domain,expected.error.domain);
  fixture_same_string(actual.error.code,expected.error.code);
  fixture_same_string(actual.error.message,expected.error.message);
}
static uint32_t ku_task_control_init(KuTaskControlV1* control,size_t bytes,uint32_t abi,
    const KuTaskControlOpsV1* operations,void* context,KuTaskControlOwnerV1* owner) {
  if (operations==&ku_task_1_ops && fixture_fault==FIXTURE_CONTROL_ABI) {
    uint32_t result=fixture_real_ku_task_control_init(control,bytes,abi+1u,operations,context,owner);
    CHECK(result==KU_TASK_CONTROL_ABI_MISMATCH && !owner->lease.control);
    CHECK(ku_task_frame_zero_bytes(control,bytes));
    return result;
  }
  return fixture_real_ku_task_control_init(control,bytes,abi,operations,context,owner);
}
static uint32_t ku_task_frame_1_init(void* storage,size_t bytes,uint32_t abi,KuResult_str* arg) {
  if (fixture_fault==FIXTURE_FRAME_ABI) {
    uint32_t result=fixture_real_ku_task_frame_1_init(storage,bytes,abi+1u,arg);
    CHECK(result==KU_TASK_FRAME_ABI_MISMATCH && ku_task_frame_zero_bytes(storage,bytes));
    return result;
  }
  return fixture_real_ku_task_frame_1_init(storage,bytes,abi,arg);
}
static uint32_t ku_task_control_poll(const KuTaskControlLeaseV1* lease) {
  int private_child=lease->control->operations.resume==ku_task_1_resume
      && !((KuTaskInstance_1*)lease->control->context)->registered;
  if (private_child) {
    CHECK(ku_task_control_atomic_load(&lease->control->references)==2u);
    CHECK(ku_task_control_atomic_load(&lease->control->lifecycle_pin)==1u);
  }
  uint32_t status=fixture_real_ku_task_control_poll(lease);
  if (private_child) {
    CHECK(status==KU_TASK_CONTROL_CANCELLED);
    CHECK(ku_task_control_atomic_load(&lease->control->references)==1u);
    CHECK(!ku_task_control_atomic_load(&lease->control->lifecycle_pin));
    fixture_private_polls++;
  }
  return status;
}
static uint32_t ku_task_control_owner_drop(KuTaskControlOwnerV1* owner,uint64_t deadline) {
  int private_child=owner->lease.control->operations.resume==ku_task_1_resume
      && !((KuTaskInstance_1*)owner->lease.control->context)->registered;
  if (private_child) CHECK(ku_task_control_atomic_load(&owner->lease.control->references)==1u);
  uint32_t status=fixture_real_ku_task_control_owner_drop(owner,deadline);
  if (private_child) {
    CHECK(status==KU_TASK_CONTROL_OK && !owner->lease.control);
    fixture_private_drops++;
  }
  return status;
}
static uint32_t ku_task_control_finish_poll(KuTaskControlV1* control,uint32_t terminal) {
  int parent=control->operations.resume==ku_task_0_resume;
  uint32_t status=fixture_real_ku_task_control_finish_poll(control,terminal);
  if (parent) CHECK(ku_test_event_set(&fixture_parent_terminal));
  return status;
}
static uint32_t ku_task_driver_reserve_child(KuTaskDriverV1* driver,
    const KuTaskDriverStartChargeV1* source,size_t bytes,KuTaskDriverTicketV1* output) {
  uint32_t status=fixture_real_ku_task_driver_reserve_child(driver,source,bytes,output);
  CHECK(status==KU_TASK_DRIVER_OK && source->owned_bytes==31u);
  fixture_reserves++; fixture_child_ticket=*output; fixture_source=*source;
  CHECK(!ku_task_driver_lock(driver));
  KuTaskDriverSlotV1* parent=ku_task_driver_find(&source->parent);
  KuTaskDriverSlotV1* child=ku_task_driver_find(output);
  CHECK(parent && child && parent->state==KU_TASK_DRIVER_RUNNING);
  CHECK(child->state==KU_TASK_DRIVER_BUILDING && child->charged_bytes==sizeof(KuTaskInstance_1));
  CHECK(parent->charged_bytes==sizeof(KuTaskInstance_0)+31u);
  CHECK(driver->reserved_bytes==sizeof(KuTaskInstance_0)+sizeof(KuTaskInstance_1)+31u);
  fixture_sum_locked(driver); CHECK(!ku_task_driver_unlock(driver));
  fixture_barrier(FIXTURE_RESERVE);
  return status;
}
static uint32_t ku_task_driver_commit_child(const KuTaskDriverTicketV1* ticket,
    KuTaskControlOwnerV1* owner,const KuTaskDriverStartChargeV1* source,size_t bytes) {
  fixture_commits++;
  CHECK(fixture_argument && fixture_empty_result(*fixture_argument));
  CHECK(ku_task_control_atomic_load(&owner->lease.control->references)==2u);
  CHECK(ku_task_control_atomic_load(&owner->lease.control->lifecycle_pin)==1u);
  CHECK(!ku_task_driver_lock(ticket->driver));
  KuTaskDriverSlotV1* parent=ku_task_driver_find(&source->parent);
  KuTaskDriverSlotV1* child=ku_task_driver_find(ticket);
  CHECK(parent && child && child->state==KU_TASK_DRIVER_BUILDING);
  CHECK(parent->charged_bytes==sizeof(KuTaskInstance_0)+31u && child->charged_bytes==bytes);
  fixture_sum_locked(ticket->driver); CHECK(!ku_task_driver_unlock(ticket->driver));
  fixture_barrier(FIXTURE_BEFORE_COMMIT);
  size_t held=fixture_fault==FIXTURE_FIRST_RETAIN ? KU_TASK_CONTROL_MAX_REFERENCES-2u
      : fixture_fault==FIXTURE_SECOND_RETAIN ? KU_TASK_CONTROL_MAX_REFERENCES-3u : 0u;
  for (size_t i=0;i<held;i++) CHECK(ku_task_control_lease_retain(&owner->lease,&fixture_quota[i])==KU_TASK_CONTROL_OK);
  uint32_t status=fixture_real_ku_task_driver_commit_child(
      fixture_fault==FIXTURE_STALE_COMMIT ? &fixture_stale : ticket,owner,source,bytes);
  /* Release the real quota leases before the real factory's private cleanup.
   * A second-retain rejection must have released its temporary registry ref. */
  if (held) CHECK(ku_task_control_atomic_load(&owner->lease.control->references)==held+2u);
  for (size_t i=0;i<held;i++) CHECK(ku_task_control_lease_release(&fixture_quota[i])==KU_TASK_CONTROL_OK);
  if (fixture_fault==FIXTURE_STALE_COMMIT) CHECK(status==KU_TASK_DRIVER_STALE);
  else if (held) CHECK(status==KU_TASK_CONTROL_LIMIT);
  else CHECK(status==KU_TASK_DRIVER_OK);
  CHECK(!ku_task_driver_lock(ticket->driver));
  parent=ku_task_driver_find(&source->parent); child=ku_task_driver_find(ticket);
  CHECK(parent && child);
  CHECK(parent->charged_bytes==sizeof(KuTaskInstance_0)+(status==KU_TASK_DRIVER_OK ? 0u : 31u));
  CHECK(child->charged_bytes==bytes+(status==KU_TASK_DRIVER_OK ? 31u : 0u));
  fixture_sum_locked(ticket->driver); CHECK(!ku_task_driver_unlock(ticket->driver));
  if (status==KU_TASK_DRIVER_OK) {
    CHECK(ku_task_control_lease_retain(&owner->lease,&fixture_child_observer)==KU_TASK_CONTROL_OK);
    fixture_barrier(FIXTURE_AFTER_COMMIT);
  } else CHECK(ku_task_control_atomic_load(&owner->lease.control->references)==2u);
  return status;
}
static uint32_t ku_task_1_try_start_impl(KuTaskDriverV1* driver,
    const KuTaskDriverStartChargeV1* source,KuResult_str* input,KuTaskHandle_1* output) {
  CHECK(source && !fixture_factory_calls++);
  KuResult_str before=*input; /* Non-owning observation, never dropped. */
  fixture_argument=input;
  uint32_t status=fixture_real_ku_task_1_try_start_impl(driver,source,input,output);
  fixture_lowlevel_status=status;
  if (status!=KU_TASK_DRIVER_OK) {
    fixture_same_result(*input,before);
    CHECK(!output->owner.lease.control && !output->ticket.driver);
    CHECK(!ku_task_driver_lock(driver));
    KuTaskDriverSlotV1* parent=ku_task_driver_find(&source->parent);
    CHECK(parent && parent->charged_bytes==sizeof(KuTaskInstance_0)+31u);
    CHECK(!driver->building && driver->resident==1u && driver->reserved_bytes==parent->charged_bytes);
    fixture_sum_locked(driver); CHECK(!ku_task_driver_unlock(driver));
    FixtureLedger ledger=fixture_ledger();
    CHECK(ledger.allocations==4u+fixture_input_buffers && ledger.bytes==fixture_fixed+sizeof(KuTaskInstance_0)+31u);
    fixture_barrier(FIXTURE_AFTER_ROLLBACK);
  } else CHECK(fixture_empty_result(*input) && output->owner.lease.control);
  return status;
}
static void fixture_child_cleanup_observe(void* raw,uint32_t reason,void* budget) {
  (void)raw; fixture_cleanups++;
  fixture_cleanup_reason=reason;
  fixture_cleanup_deadline=ku_task_control_cleanup_deadline((KuTaskControlV1*)budget);
}
static void fixture_child_dispose_observe(void* raw) {
  (void)raw; fixture_child_disposes++;
}
static void fixture_child_resume_observe(void* raw) {
  (void)raw; fixture_child_resumes++;
}
static KuString fixture_owned(size_t capacity,const char* text) {
  size_t length=strlen(text); CHECK(length<=capacity);
  uint8_t* buffer=(uint8_t*)malloc(capacity); CHECK(buffer);
  memcpy(buffer,text,length);
  return (KuString){buffer,length,capacity,KU_STRING_OWNED};
}
static KuResult_str fixture_input(int error) {
  KuResult_str input={0}; input.ok=!error; fixture_input_buffers=error ? 3u : 1u;
  if (error) input.error=(KuError){fixture_owned(7u,"domain"),fixture_owned(11u,"code"),fixture_owned(13u,"message")};
  else input.value=fixture_owned(31u,"payload");
  return input;
}
typedef struct FixtureRuntime { KuTaskDriverV1* driver; KuTaskDriverSlotV1* slots; size_t* ring; } FixtureRuntime;
static void fixture_begin(FixtureRuntime* runtime,unsigned fault,unsigned pause) {
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes);
  fixture_fault=fault; fixture_pause=pause;
  fixture_factory_calls=fixture_reserves=fixture_commits=0;
  fixture_private_polls=fixture_private_drops=fixture_child_disposes=0;
  fixture_child_resumes=fixture_cleanups=0; fixture_lowlevel_status=UINT32_MAX;
  fixture_cleanup_reason=0; fixture_cleanup_deadline=UINT64_MAX;
  fixture_argument=NULL; fixture_child_observer=(KuTaskControlLeaseV1){0};
  fixture_child_ticket=(KuTaskDriverTicketV1){0}; fixture_source=(KuTaskDriverStartChargeV1){0};
  CHECK(ku_test_event_init(&fixture_entered)); CHECK(ku_test_event_init(&fixture_proceed));
  CHECK(ku_test_event_init(&fixture_parent_terminal)); CHECK(ku_test_event_init(&fixture_exited));
  runtime->driver=(KuTaskDriverV1*)calloc(1,sizeof(*runtime->driver));
  runtime->slots=(KuTaskDriverSlotV1*)calloc(4,sizeof(*runtime->slots));
  runtime->ring=(size_t*)calloc(4,sizeof(*runtime->ring));
  CHECK(runtime->driver && runtime->slots && runtime->ring);
  fixture_fixed=sizeof(*runtime->driver)+4u*(sizeof(*runtime->slots)+sizeof(*runtime->ring));
  CHECK(ku_task_driver_init(runtime->driver,sizeof(*runtime->driver),KU_TASK_DRIVER_ABI_VERSION,
      runtime->slots,4,runtime->ring,4,fixture_fixed+sizeof(KuTaskInstance_0)+sizeof(KuTaskInstance_1)+31u)==KU_TASK_DRIVER_OK);
  /* A real historical ticket, invalidated by successful rollback and possibly
   * reused by Parent; never a fabricated generation used to forge success. */
  KuTaskDriverTicketV1 reserve={0};
  CHECK(ku_task_driver_reserve(runtime->driver,sizeof(KuTaskInstance_1),&reserve)==KU_TASK_DRIVER_OK);
  fixture_stale=reserve; CHECK(ku_task_driver_rollback(&reserve)==KU_TASK_DRIVER_OK);
}
static void fixture_finish(FixtureRuntime* runtime,KuTaskValueV1* root,
    KuTaskControlLeaseV1* cancel,uint64_t deadline,int expected_fault,int was_closing) {
  CHECK(ku_task_value_drop(root,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_control_lease_release(cancel)==KU_TASK_CONTROL_OK);
  if (fixture_child_observer.control) CHECK(ku_task_control_lease_release(&fixture_child_observer)==KU_TASK_CONTROL_OK);
  if (!was_closing) {
    uint32_t shutdown=ku_task_driver_shutdown(runtime->driver,deadline);
    CHECK(shutdown==(expected_fault ? KU_TASK_DRIVER_INTERNAL : KU_TASK_DRIVER_OK));
  }
  CHECK(ku_test_event_wait(&fixture_exited,2000));
  KuTaskDriverSnapshotV1 snapshot={0};
  CHECK(ku_task_driver_snapshot(runtime->driver,&snapshot)==KU_TASK_DRIVER_OK);
  CHECK(!snapshot.resident && !snapshot.building && !snapshot.reserved_bytes && !snapshot.running && !snapshot.queued);
  CHECK(snapshot.fault==(expected_fault ? KU_TASK_DRIVER_INTERNAL : 0u));
  CHECK(snapshot.clock_fault==(unsigned)(expected_fault==2));
  CHECK(!ku_task_driver_lock(runtime->driver)); fixture_sum_locked(runtime->driver);
  if (was_closing) CHECK(runtime->driver->shutdown_deadline==deadline);
  CHECK(!ku_task_driver_unlock(runtime->driver));
#if defined(_WIN32)
  CHECK(WaitForSingleObject(runtime->driver->thread,2000)==WAIT_OBJECT_0);
#else
  alarm(2);
#endif
  CHECK(ku_task_driver_destroy(runtime->driver)==KU_TASK_DRIVER_OK);
#if !defined(_WIN32)
  alarm(0);
#endif
  free(runtime->ring); free(runtime->slots); free(runtime->driver);
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes && !fixture_ledger().overflow);
  CHECK(ku_test_event_destroy(&fixture_entered)); CHECK(ku_test_event_destroy(&fixture_proceed));
  CHECK(ku_test_event_destroy(&fixture_parent_terminal)); CHECK(ku_test_event_destroy(&fixture_exited));
  ku_task_control_atomic_store(&fixture_bad_clock,0);
}
static void fixture_fault_case(unsigned fault,int error) {
  FixtureRuntime runtime={0};
  int fatal=fault<=FIXTURE_STALE_COMMIT;
  fixture_begin(&runtime,fault,fatal ? FIXTURE_AFTER_ROLLBACK : 0u);
  KuResult_str input=fixture_input(error); KuTaskValueV1 root={0}; KuTaskControlLeaseV1 cancel={0};
  CHECK(ku_task_0_start_value(runtime.driver,&input,&root)==KU_TASK_DRIVER_OK && root.tag==KU_TASK_VALUE_LIVE);
  CHECK(fixture_empty_result(input));
  CHECK(ku_task_control_lease_retain(&root.owner.lease,&cancel)==KU_TASK_CONTROL_OK);
  uint64_t deadline=fixture_original_now()+1000u;
  if (fatal) {
    CHECK(ku_test_event_wait(&fixture_entered,2000));
    CHECK(ku_task_driver_request_cancel(&root.ticket,&cancel,KU_TASK_CONTROL_CANCELLED,deadline)==KU_TASK_CONTROL_OK);
    CHECK(ku_test_event_set(&fixture_proceed));
  }
  CHECK(ku_test_event_wait(&fixture_parent_terminal,2000));
  CHECK(ku_task_control_status(&cancel)==(fatal ? KU_TASK_CONTROL_CANCELLED : KU_TASK_CONTROL_FAILED));
  if (!fatal) {
    KuTaskAdapterOutcomeV1 output={0}; CHECK(ku_task_value_take(&root,NULL,&output)==KU_TASK_CONTROL_OK);
    CHECK(output.exit_class==KU_TASK_EXIT_USER_RESULT && !output.value.string.ok);
    KuError failure=output.value.string.error;
    CHECK(failure.domain.len==4u && !memcmp(failure.domain.ptr,"task",4u));
    CHECK(failure.code.len==14u && !memcmp(failure.code.ptr,"too_many_tasks",14u));
    ku_task_outcome_drop(&output);
  }
  fixture_finish(&runtime,&root,&cancel,deadline,fatal,0);
  CHECK(fixture_factory_calls==1u && fixture_reserves==1u && !fixture_child_resumes);
  CHECK(fixture_commits==(fault>=FIXTURE_STALE_COMMIT ? 1u : 0u));
  CHECK(fixture_private_polls==(fault==FIXTURE_CONTROL_ABI ? 0u : 1u));
  CHECK(fixture_private_drops==fixture_private_polls && fixture_child_disposes==fixture_private_polls);
  CHECK(fixture_cleanups==fixture_private_polls);
  if (fixture_private_polls) CHECK(fixture_cleanup_reason==KU_TASK_CONTROL_CANCELLED && !fixture_cleanup_deadline);
  CHECK(fixture_lowlevel_status==(fault==FIXTURE_STALE_COMMIT ? KU_TASK_DRIVER_STALE
      : fault>=FIXTURE_FIRST_RETAIN ? KU_TASK_CONTROL_LIMIT : KU_TASK_CONTROL_ABI_MISMATCH));
}
static void fixture_fatal_idle(FixtureRuntime* runtime,const KuTaskValueV1* root,
    KuResult_str original) {
  /* Unlike the earlier cancellation-boundary cases, fatal Start itself must
   * stop normal execution. A driver mutex/idle handshake observes the callback
   * return before inspecting its non-atomic witnesses; no sleep guesses. */
  CHECK(ku_task_driver_wait_idle(runtime->driver,fixture_original_now()+2000u)==KU_TASK_DRIVER_INTERNAL);
  KuTaskDriverSnapshotV1 snapshot={0};
  CHECK(ku_task_driver_snapshot(runtime->driver,&snapshot)==KU_TASK_DRIVER_OK);
  CHECK(snapshot.polls==1u && snapshot.worker_waiting && !snapshot.worker_exited);
  CHECK(snapshot.resident==1u && snapshot.parked==1u);
  CHECK(!snapshot.building && !snapshot.running && !snapshot.queued && !snapshot.retiring);
  CHECK(snapshot.reserved_bytes==sizeof(KuTaskInstance_0)+31u);
  CHECK(snapshot.fault==KU_TASK_DRIVER_INTERNAL && !snapshot.clock_fault && !snapshot.closing);
  CHECK(ku_task_control_atomic_load(&root->owner.lease.control->phase)==KU_TASK_CONTROL_LIVE);
  CHECK(!ku_task_driver_lock(runtime->driver));
  KuTaskDriverSlotV1* slot=ku_task_driver_find(&root->ticket);
  CHECK(slot && slot->state!=KU_TASK_DRIVER_RUNNING && slot->state!=KU_TASK_DRIVER_QUEUED);
  CHECK(slot->driver_lease.control && slot->execution_lease.control);
  fixture_sum_locked(runtime->driver); CHECK(!ku_task_driver_unlock(runtime->driver));
  CHECK(fixture_factory_calls==1u && fixture_reserves==1u && !fixture_child_resumes);
  CHECK(fixture_argument); fixture_same_result(*fixture_argument,original);
  FixtureLedger ledger=fixture_ledger();
  CHECK(ledger.allocations==4u+fixture_input_buffers);
  CHECK(ledger.bytes==fixture_fixed+sizeof(KuTaskInstance_0)+31u && !ledger.overflow);
}
static void fixture_fatal_root_cleanup_case(unsigned fault,int error) {
  FixtureRuntime runtime={0}; fixture_begin(&runtime,fault,FIXTURE_AFTER_ROLLBACK);
  KuResult_str input=fixture_input(error),original=input; /* Non-owning witness. */
  KuTaskValueV1 root={0}; KuTaskControlLeaseV1 observer={0};
  CHECK(ku_task_0_start_value(runtime.driver,&input,&root)==KU_TASK_DRIVER_OK && root.tag==KU_TASK_VALUE_LIVE);
  CHECK(fixture_empty_result(input));
  CHECK(ku_task_control_lease_retain(&root.owner.lease,&observer)==KU_TASK_CONTROL_OK);
  CHECK(ku_test_event_wait(&fixture_entered,2000));
  /* A genuine wake racing the failing callback is not permission to retry the
   * frame's already-executed moves. In particular, there is NO cancellation at
   * AFTER_ROLLBACK: the source root has not seen the eventual sticky fault yet. */
  CHECK(ku_task_driver_wake(&root.ticket)==KU_TASK_DRIVER_OK);
  CHECK(ku_test_event_set(&fixture_proceed));
  fixture_fatal_idle(&runtime,&root,original);
  /* A later ordinary wake must likewise not re-enter the still-LIVE frame.
   * Existing !fixture_factory_calls++ rejects a second real Start immediately. */
  CHECK(ku_task_driver_wake(&root.ticket)==KU_TASK_DRIVER_OK);
  fixture_fatal_idle(&runtime,&root,original);
  /* Even an unsuccessful real take runs wrapper_end and its notification.
   * That notification must not replay a faulted, still-LIVE source frame. */
  KuTaskAdapterOutcomeV1 output; memset(&output,0,sizeof(output));
  CHECK(ku_task_value_take(&root,NULL,&output)==KU_TASK_CONTROL_PENDING);
  CHECK(ku_task_frame_zero_bytes(&output,sizeof(output)));
  CHECK(root.tag==KU_TASK_VALUE_LIVE && root.owner.lease.control);
  fixture_fatal_idle(&runtime,&root,original);
  CHECK(ku_task_driver_wait_result(&root.ticket,&root.owner.lease,UINT64_MAX)==KU_TASK_DRIVER_INTERNAL);
  /* Match the external root's failure path: owner drop requests cancellation,
   * then real shutdown drives cleanup. Do not clear the sticky fault or forge
   * a terminal result to obtain zero accounting. */
  fixture_finish(&runtime,&root,&observer,fixture_original_now()+1000u,1,0);
  CHECK(fixture_factory_calls==1u && fixture_reserves==1u && !fixture_child_resumes);
  CHECK(fixture_commits==(fault==FIXTURE_STALE_COMMIT ? 1u : 0u));
  CHECK(fixture_private_polls==(fault==FIXTURE_CONTROL_ABI ? 0u : 1u));
  CHECK(fixture_private_drops==fixture_private_polls && fixture_child_disposes==fixture_private_polls);
  CHECK(fixture_cleanups==fixture_private_polls);
  if (fixture_private_polls) CHECK(fixture_cleanup_reason==KU_TASK_CONTROL_CANCELLED && !fixture_cleanup_deadline);
  CHECK(fixture_lowlevel_status==(fault==FIXTURE_STALE_COMMIT ? KU_TASK_DRIVER_STALE : KU_TASK_CONTROL_ABI_MISMATCH));
}
static void fixture_cancel_case(unsigned point,unsigned action,int error) {
  FixtureRuntime runtime={0}; fixture_begin(&runtime,FIXTURE_NORMAL,point);
  KuResult_str input=fixture_input(error); KuTaskValueV1 root={0}; KuTaskControlLeaseV1 cancel={0};
  CHECK(ku_task_0_start_value(runtime.driver,&input,&root)==KU_TASK_DRIVER_OK && root.tag==KU_TASK_VALUE_LIVE);
  CHECK(fixture_empty_result(input));
  CHECK(ku_task_control_lease_retain(&root.owner.lease,&cancel)==KU_TASK_CONTROL_OK);
  CHECK(ku_test_event_wait(&fixture_entered,2000));
  uint32_t reason=action==FIXTURE_CANCEL ? KU_TASK_CONTROL_CANCELLED : KU_TASK_CONTROL_TIMED_OUT;
  uint64_t deadline=fixture_original_now()+1000u;
  CHECK(ku_task_driver_request_cancel(&root.ticket,&cancel,reason,deadline)==KU_TASK_CONTROL_OK);
  CHECK(ku_task_driver_request_cancel(&root.ticket,&cancel,
      reason==KU_TASK_CONTROL_CANCELLED ? KU_TASK_CONTROL_TIMED_OUT : KU_TASK_CONTROL_CANCELLED,
      deadline+1000u)==KU_TASK_CONTROL_OK);
  CHECK(ku_task_control_cleanup_deadline(cancel.control)==deadline);
  if (action==FIXTURE_CLOSING) {
    deadline=fixture_original_now();
    CHECK(ku_task_driver_shutdown(runtime.driver,deadline)==KU_TASK_DRIVER_SHUTDOWN_TIMEOUT);
    CHECK(ku_task_driver_destroy(runtime.driver)==KU_TASK_DRIVER_PENDING);
  } else if (action==FIXTURE_CLOCK) {
    ku_task_control_atomic_store(&fixture_bad_clock,1);
    CHECK(ku_task_driver_shutdown(runtime.driver,deadline)==KU_TASK_DRIVER_INTERNAL);
    deadline=0;
    CHECK(ku_task_driver_destroy(runtime.driver)==KU_TASK_DRIVER_PENDING);
  }
  CHECK(ku_test_event_set(&fixture_proceed));
  CHECK(ku_test_event_wait(&fixture_parent_terminal,2000));
  CHECK(ku_task_control_status(&cancel)==reason);
  CHECK(ku_task_control_cleanup_deadline(cancel.control)==deadline);
  if (fixture_child_observer.control) {
    /* Parent may finish an expired scope before child ACK. The real registry
     * and observer keep child alive; final assertions run after worker exit. */
    KuTaskControlV1* child=fixture_child_observer.control;
    CHECK(ku_task_control_is_requested(ku_task_control_atomic_load(&child->phase))
        || ku_task_control_is_terminal(ku_task_control_atomic_load(&child->phase)));
  }
  fixture_finish(&runtime,&root,&cancel,deadline,action==FIXTURE_CLOCK ? 2 : 0,action>=FIXTURE_CLOSING);
  CHECK(fixture_factory_calls==1u && fixture_reserves==1u && fixture_commits==1u);
  CHECK(fixture_lowlevel_status==KU_TASK_DRIVER_OK && !fixture_private_polls && !fixture_private_drops);
  CHECK(!fixture_child_resumes && fixture_child_disposes==1u && fixture_cleanups==1u);
  CHECK(fixture_cleanup_reason==KU_TASK_CONTROL_CANCELLED && fixture_cleanup_deadline==deadline);
}
int main(void) {
  CHECK(KU_TASK_FRAME_ABI_VERSION==2u && KU_TASK_DRIVER_ABI_VERSION==4u);
  ku_task_control_atomic_init(&fixture_bad_clock,0);
  for (int error=0;error<2;error++) {
    for (unsigned fault=FIXTURE_CONTROL_ABI;fault<=FIXTURE_SECOND_RETAIN;fault++) fixture_fault_case(fault,error);
    for (unsigned fault=FIXTURE_CONTROL_ABI;fault<=FIXTURE_STALE_COMMIT;fault++) fixture_fatal_root_cleanup_case(fault,error);
    for (unsigned point=FIXTURE_RESERVE;point<=FIXTURE_AFTER_COMMIT;point++)
      for (unsigned action=FIXTURE_CANCEL;action<=FIXTURE_CLOCK;action++) fixture_cancel_case(point,action,error);
  }
  puts("task-start-failures-ok");
  return 0;
}
"#;
