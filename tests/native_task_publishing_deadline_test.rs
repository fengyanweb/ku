//! A cancellation reservation must not strand a shorter cleanup deadline.
//! This is admitted source -> generated frames/adapter -> real R2/driver.
//! Hooks only pause genuine publication and child progress; no phase, ACK,
//! receipt, outcome, or deadline propagation is implemented by the fixture.
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
        task::{self, TaskLimits},
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
    assert_eq!(
        source.matches(anchor).count(),
        1,
        "publishing hook: {anchor}"
    );
    source.replacen(anchor, replacement, 1)
}

#[test]
fn native_task_publishing_cancel_tightens_unacked_child_after_timeout_result_race() {
    let source = r#"
async fn Parent(value: str): int! {
    held = Held(value)
    return ok(7)
}
async fn Held(value: str): int! { return ok(1) }
async fn main(): null! { return ok(null) }
"#;
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let native = task_lower::lower_program(&ast).unwrap();
    let plan = task::verify_and_plan(&native.tasks, TaskLimits::default()).unwrap();
    assert!(plan.functions[0].exit_bridge && plan.functions[0].hosted);
    assert_eq!(native.tasks.functions[0].name, "Parent");
    assert_eq!(native.tasks.functions[1].name, "Held");
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
        "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\nstatic void fixture_record_free(void*);\n{ledger}\nstatic uint32_t fixture_hold(void*,int);\nstatic void fixture_before_terminal(void*);\nstatic void fixture_before_publish(void*);\nstatic void fixture_worker_exited(void);\n"
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
        "static uint32_t ku_task_1_resume(void* raw) {",
        "static uint32_t ku_task_1_resume(void* raw) {\n  uint32_t held=fixture_hold(raw,0);\n  if (held!=UINT32_MAX) return held;",
    );
    let generated = replace_once(
        generated,
        "static uint32_t ku_task_1_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {",
        "static uint32_t ku_task_1_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {\n  uint32_t held=fixture_hold(raw,1);\n  if (held!=UINT32_MAX) return held;",
    );
    // Locate the real Parent adapter only. The injected callback does not
    // supply CONTROL_FAILED: it pauses immediately before the existing return.
    let begin = generated
        .find("static uint32_t ku_task_0_resume(void* raw) {")
        .unwrap();
    let end = generated[begin..]
        .find("static uint32_t ku_task_0_cleanup(")
        .map(|offset| begin + offset)
        .unwrap();
    let original = &generated[begin..end];
    let observed = replace_once(
        original.to_owned(),
        "      return KU_TASK_CONTROL_FAILED;",
        "      fixture_before_terminal(instance);\n      return KU_TASK_CONTROL_FAILED;",
    );
    let generated = replace_once(generated.clone(), original, &observed);
    let generated = replace_once(
        generated,
        "      ku_task_control_deadline_store(&control->cleanup_deadline_ms, absolute_deadline_ms);",
        "      fixture_before_publish(control);\n      ku_task_control_deadline_store(&control->cleanup_deadline_ms, absolute_deadline_ms);",
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
    let directory = TempDir::new("native-task-publishing-deadline");
    let path = directory.path().join("program.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute native cancellation publication"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("publication fixture ends under the real process watchdog");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-publishing-deadline-ok\n"
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
static KuAtomicRefcount fixture_child_hold;
static KuTestEvent fixture_terminal,fixture_terminal_proceed;
static KuTestEvent fixture_publishing,fixture_publish_proceed,fixture_exit_event;
static void* fixture_parent_target;
static uintptr_t fixture_input_identity;
static unsigned fixture_input_frees,fixture_terminal_calls,fixture_publish_calls,fixture_cleanup_calls;

static void fixture_record_free(void* pointer) {
  /* Shared allocation lock is held. Identity was saved before ownership move. */
  if (pointer && (uintptr_t)pointer==fixture_input_identity) CHECK(!fixture_input_frees++);
}
static void fixture_worker_exited(void) { CHECK(ku_test_event_set(&fixture_exit_event)); }
static uint32_t fixture_hold(void* raw,int cleanup) {
  KuTaskInstance_1* instance=(KuTaskInstance_1*)raw;
  if (cleanup) fixture_cleanup_calls++;
  if (!cleanup || ku_task_control_atomic_load(&fixture_child_hold)) {
    CHECK(ku_task_driver_set_intent(&instance->ticket,KU_TASK_DRIVER_WAIT)==KU_TASK_DRIVER_OK);
    return KU_TASK_CONTROL_PENDING;
  }
  return UINT32_MAX; /* Continue the real generated cleanup and actual ACK. */
}
static void fixture_before_terminal(void* raw) {
  CHECK(raw==fixture_parent_target && !fixture_terminal_calls++);
  KuTaskInstance_0* instance=(KuTaskInstance_0*)raw;
  CHECK(instance->drain_failure==KU_TASK_DRIVER_CLEANUP_TIMEOUT);
  CHECK(instance->payload_initialized && !instance->payload.ok);
  CHECK(instance->exit_class==KU_TASK_EXIT_RUNTIME_FAILURE && instance->receipt_mask
      && !(instance->receipt_mask & (instance->receipt_mask-1u)));
  CHECK(ku_test_event_set(&fixture_terminal));
  CHECK(ku_test_event_wait(&fixture_terminal_proceed,2000u));
}
static void fixture_before_publish(void* raw) {
  if (raw!=fixture_parent_target) return;
  CHECK(!fixture_publish_calls++);
  CHECK(ku_task_control_atomic_load(&((KuTaskControlV1*)raw)->phase)==KU_TASK_CONTROL_PUBLISHING_CANCEL);
  CHECK(ku_test_event_set(&fixture_publishing));
  CHECK(ku_test_event_wait(&fixture_publish_proceed,2000u));
}
static KuTaskDriverSnapshotV1 fixture_idle(KuTaskDriverV1* driver) {
  /* Observation budget, never a new task cleanup deadline. The virtual
   * clock is bounded independently by the existing real process watchdog. */
  CHECK(ku_task_driver_wait_idle(driver,ku_task_driver_now_ms()+2000u)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot={0};
  CHECK(ku_task_driver_snapshot(driver,&snapshot)==KU_TASK_DRIVER_OK);
  CHECK(!snapshot.fault && !snapshot.running && !snapshot.queued && !snapshot.building);
  CHECK(snapshot.worker_waiting || snapshot.worker_exited);
  return snapshot;
}
typedef struct FixtureCancel {
  KuTaskDriverTicketV1 ticket;
  KuTaskControlLeaseV1 lease;
  uint64_t deadline;
  uint32_t status;
} FixtureCancel;
static int fixture_cancel_thread(void* raw) {
  FixtureCancel* cancel=(FixtureCancel*)raw;
  cancel->status=ku_task_driver_request_cancel(&cancel->ticket,&cancel->lease,
      KU_TASK_CONTROL_CANCELLED,cancel->deadline);
  return 0;
}
int main(void) {
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes);
  ku_task_control_deadline_init(&fixture_clock);
  ku_task_control_deadline_store(&fixture_clock,ku_test_real_now_ms()+100000u);
  ku_task_control_atomic_init(&fixture_child_hold,1u);
  CHECK(ku_test_event_init(&fixture_terminal));
  CHECK(ku_test_event_init(&fixture_terminal_proceed));
  CHECK(ku_test_event_init(&fixture_publishing));
  CHECK(ku_test_event_init(&fixture_publish_proceed));
  CHECK(ku_test_event_init(&fixture_exit_event));
  KuTaskDriverV1* driver=(KuTaskDriverV1*)calloc(1,sizeof(*driver));
  KuTaskDriverSlotV1* slots=(KuTaskDriverSlotV1*)calloc(2,sizeof(*slots));
  size_t* ring=(size_t*)calloc(2,sizeof(*ring)); CHECK(driver && slots && ring);
  size_t fixed=sizeof(*driver)+2u*(sizeof(*slots)+sizeof(*ring));
  size_t instances=sizeof(KuTaskInstance_0)+sizeof(KuTaskInstance_1);
  CHECK(ku_task_driver_init(driver,sizeof(*driver),KU_TASK_DRIVER_ABI_VERSION,
      slots,2,ring,2,fixed+instances+31u)==KU_TASK_DRIVER_OK);
  uint8_t* buffer=(uint8_t*)malloc(31u); CHECK(buffer); memcpy(buffer,"payload",7u);
  fixture_input_identity=(uintptr_t)buffer;
  KuString input={buffer,7u,31u,KU_STRING_OWNED};
  KuTaskValueV1 root={0};
  CHECK(ku_task_0_start_value(driver,&input,&root)==KU_TASK_DRIVER_OK && !input.ptr);
  KuTaskDriverSnapshotV1 first=fixture_idle(driver);
  CHECK(first.resident==2u && first.reserved_bytes==instances+31u);
  KuTaskInstance_0* parent=(KuTaskInstance_0*)root.owner.lease.control;
  fixture_parent_target=parent; /* Worker is idle; immutable before wake/start. */
  KuTaskControlLeaseV1 observer={0};
  CHECK(!ku_task_driver_lock(driver));
  for (size_t i=0;i<2u;i++) {
    if (slots[i].binding && slots[i].binding->operations.resume==ku_task_1_resume) {
      CHECK(!observer.control);
      CHECK(ku_task_control_lease_retain(&slots[i].driver_lease,&observer)==KU_TASK_CONTROL_OK);
    }
  }
  CHECK(!ku_task_driver_unlock(driver)); CHECK(observer.control);
  KuTaskInstance_1* child=(KuTaskInstance_1*)observer.control;
  CHECK(parent->payload_initialized && parent->payload.ok && parent->payload.value==7);
  CHECK(parent->drain_started && parent->receipt_mask
      && !(parent->receipt_mask & (parent->receipt_mask-1u)) && !parent->drain_failure);
  CHECK(parent->wait.driver && !fixture_input_frees);
  uint64_t original=parent->drain_deadline, shorter=original-1u;
  CHECK(original==ku_task_driver_now_ms()+1000u);
  CHECK(ku_task_control_atomic_load(&child->control.phase)==KU_TASK_CONTROL_REQUESTED_CANCEL);
  CHECK(ku_task_control_cleanup_deadline(&child->control)==original);
  /* Source Start temporaries and named Task destinations have independent
   * declaration-ordered receipt positions. Select the actual issued receipt. */
  size_t receipt_index=0;
  while (!(parent->receipt_mask & (UINT64_C(1)<<receipt_index))) {
    receipt_index++;
    CHECK(receipt_index<sizeof(parent->receipts)/sizeof(*parent->receipts));
  }
  KuTaskDriverCleanupReceiptV1 receipt=parent->receipts[receipt_index];
  CHECK(ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_PENDING);
  /* Produce the real existing normal final-drain timeout, not a fake Result
   * or control reason. Parent pauses only after the real adapter selects it. */
  CHECK(!ku_task_driver_lock(driver));
  ku_task_control_deadline_store(&fixture_clock,original);
  (void)ku_task_driver_deadlines_locked(driver,original); ku_task_driver_signal(driver);
  CHECK(!ku_task_driver_unlock(driver));
  CHECK(ku_test_event_wait(&fixture_terminal,2000u));
  FixtureCancel cancel={0}; cancel.ticket=root.ticket; cancel.deadline=shorter;
  CHECK(ku_task_control_lease_retain(&root.owner.lease,&cancel.lease)==KU_TASK_CONTROL_OK);
  KuTestThread publisher;
  CHECK(ku_test_thread_start(&publisher,fixture_cancel_thread,&cancel));
  CHECK(ku_test_event_wait(&fixture_publishing,2000u));
  CHECK(ku_task_control_atomic_load(&parent->control.phase)==KU_TASK_CONTROL_PUBLISHING_CANCEL);
  CHECK(ku_task_control_cleanup_deadline(&child->control)==original);
  /* Let the genuine worker finish the losing completion attempt while the
   * actual cancellation publisher still owns the unpublished deadline. */
  CHECK(ku_test_event_set(&fixture_terminal_proceed)); fixture_idle(driver);
  CHECK(ku_task_control_atomic_load(&parent->control.phase)==KU_TASK_CONTROL_PUBLISHING_CANCEL);
  CHECK(ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_PENDING);
  CHECK(!fixture_input_frees);
  CHECK(ku_test_event_set(&fixture_publish_proceed));
  CHECK(ku_test_thread_join(&publisher,2000u) && !publisher.outcome);
  CHECK(cancel.status==KU_TASK_CONTROL_OK); fixture_idle(driver);
  CHECK(ku_task_control_atomic_load(&parent->control.phase)==KU_TASK_CONTROL_CANCELLED);
  CHECK(ku_task_control_cleanup_deadline(&parent->control)==shorter);
  CHECK(ku_task_control_atomic_load(&child->control.phase)==KU_TASK_CONTROL_REQUESTED_CANCEL);
  uint64_t observed_child_deadline=ku_task_control_cleanup_deadline(&child->control);
  CHECK(ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_PENDING);
  CHECK(!fixture_input_frees && fixture_terminal_calls==1u && fixture_publish_calls==1u);
  /* Complete cleanup before reporting the regression, even on the old path.
   * Late ACK does not retroactively turn the expired cleanup into success. */
  ku_task_control_atomic_store(&fixture_child_hold,0);
  CHECK(ku_task_driver_wake(&child->ticket)==KU_TASK_DRIVER_OK); fixture_idle(driver);
  CHECK(ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_CLEANUP_ACK);
  CHECK(ku_task_control_atomic_load(&child->control.phase)==KU_TASK_CONTROL_CANCELLED);
  CHECK(ku_task_control_cleanup_deadline(&child->control)==observed_child_deadline);
  CHECK(child->control.frame_destroyed && fixture_input_frees==1u);
  CHECK(fixture_cleanup_calls>=2u && fixture_cleanup_calls<=4u);
  CHECK(ku_task_control_atomic_load(&parent->control.phase)==KU_TASK_CONTROL_CANCELLED);
  CHECK(ku_task_control_lease_release(&cancel.lease)==KU_TASK_CONTROL_OK);
  CHECK(ku_task_value_drop(&root,shorter)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_control_lease_release(&observer)==KU_TASK_CONTROL_OK);
  KuTaskDriverSnapshotV1 empty=fixture_idle(driver);
  CHECK(!empty.resident && !empty.reserved_bytes);
  CHECK(ku_task_driver_shutdown(driver,original)==KU_TASK_DRIVER_SHUTDOWN_TIMEOUT);
  CHECK(driver->shutdown_deadline==original);
  CHECK(ku_test_event_wait(&fixture_exit_event,2000u));
#if defined(_WIN32)
  CHECK(WaitForSingleObject(driver->thread,2000u)==WAIT_OBJECT_0);
#else
  alarm(2);
#endif
  CHECK(ku_task_driver_destroy(driver)==KU_TASK_DRIVER_OK);
#if !defined(_WIN32)
  alarm(0);
#endif
  CHECK(ku_test_event_destroy(&fixture_terminal));
  CHECK(ku_test_event_destroy(&fixture_terminal_proceed));
  CHECK(ku_test_event_destroy(&fixture_publishing));
  CHECK(ku_test_event_destroy(&fixture_publish_proceed));
  CHECK(ku_test_event_destroy(&fixture_exit_event));
  free(ring); free(slots); free(driver);
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes && !fixture_ledger().overflow);
  if (observed_child_deadline!=shorter) {
    fprintf(stderr,"short cleanup deadline not propagated: expected=%llu actual=%llu original=%llu\n",
        (unsigned long long)shorter,(unsigned long long)observed_child_deadline,(unsigned long long)original);
  }
  CHECK(observed_child_deadline==shorter);
  puts("task-publishing-deadline-ok"); return 0;
}
"#;
