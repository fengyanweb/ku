//! Tightening an already-requested cancellation must reach unACKed children.
//! This is admitted source -> generated frames/adapter -> real R2/driver.
//! Hooks only pause genuine cleanup completion and child progress; no phase,
//! ACK, receipt, or deadline propagation is implemented by the fixture.
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
        "cleanup commit hook: {anchor}"
    );
    source.replacen(anchor, replacement, 1)
}

#[test]
fn native_task_requested_cancel_tightens_unacked_child_before_cleanup_commit() {
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
        "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\nstatic void fixture_record_free(void*);\n{ledger}\nstatic uint32_t fixture_hold(void*,int);\nstatic void fixture_before_cleanup_commit(void*);\nstatic void fixture_worker_exited(void);\n"
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
    // Observe only the real Parent cleanup's existing success/timeout return.
    // The fixture never fabricates cleanup completion or an ACK.
    let begin = generated
        .find("static uint32_t ku_task_0_cleanup(")
        .unwrap();
    let end = generated[begin..]
        .find("static void ku_task_0_drop_frame(")
        .map(|offset| begin + offset)
        .unwrap();
    let original = &generated[begin..end];
    let observed = replace_once(
        original.to_owned(),
        "  if (status==KU_TASK_DRIVER_OK || status==KU_TASK_DRIVER_CLEANUP_TIMEOUT) return KU_TASK_CONTROL_OK;",
        "  if (status==KU_TASK_DRIVER_OK || status==KU_TASK_DRIVER_CLEANUP_TIMEOUT) { fixture_before_cleanup_commit(instance); return KU_TASK_CONTROL_OK; }",
    );
    let generated = replace_once(generated.clone(), original, &observed);
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
    let directory = TempDir::new("native-task-cleanup-commit");
    let path = directory.path().join("program.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute native cleanup deadline commit"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("cleanup commit fixture ends under the real process watchdog");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-cleanup-commit-ok\n"
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
static KuTestEvent fixture_commit,fixture_commit_proceed,fixture_exit_event;
static void* fixture_parent_target;
static uintptr_t fixture_input_identity;
static unsigned fixture_input_frees,fixture_commit_calls,fixture_cleanup_calls;

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
static void fixture_before_cleanup_commit(void* raw) {
  CHECK(raw==fixture_parent_target);
  CHECK(++fixture_commit_calls<=2u);
  KuTaskInstance_0* instance=(KuTaskInstance_0*)raw;
  CHECK(instance->drain_failure==KU_TASK_DRIVER_CLEANUP_TIMEOUT);
  CHECK(!instance->payload_initialized && instance->values_cleaned);
  CHECK(instance->receipt_mask && !(instance->receipt_mask & (instance->receipt_mask-1u)));
  CHECK(ku_task_control_atomic_load(&instance->control.phase)==KU_TASK_CONTROL_REQUESTED_CANCEL);
  if (fixture_commit_calls==1u) {
    CHECK(ku_test_event_set(&fixture_commit));
    CHECK(ku_test_event_wait(&fixture_commit_proceed,2000u));
  }
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
int main(void) {
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes);
  ku_task_control_deadline_init(&fixture_clock);
  ku_task_control_deadline_store(&fixture_clock,ku_test_real_now_ms()+100000u);
  ku_task_control_atomic_init(&fixture_child_hold,1u);
  CHECK(ku_test_event_init(&fixture_commit));
  CHECK(ku_test_event_init(&fixture_commit_proceed));
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
  CHECK(parent->drain_started && parent->receipt_mask && !(parent->receipt_mask & (parent->receipt_mask-1u)) && !parent->drain_failure);
  CHECK(parent->wait.driver && !fixture_input_frees);
  uint64_t original=parent->drain_deadline, shorter=original-1u;
  CHECK(original==ku_task_driver_now_ms()+1000u);
  CHECK(ku_task_control_atomic_load(&child->control.phase)==KU_TASK_CONTROL_REQUESTED_CANCEL);
  CHECK(ku_task_control_cleanup_deadline(&child->control)==original);
  KuTaskDriverCleanupReceiptV1 receipt={0};
  size_t receipt_count=sizeof(parent->receipts)/sizeof(parent->receipts[0]);
  for (size_t i=0;i<receipt_count;i++) {
    if (parent->receipt_mask & (UINT64_C(1)<<i)) receipt=parent->receipts[i];
  }
  CHECK(receipt.driver==driver && receipt.slot==child->ticket.slot && receipt.generation==child->ticket.generation);
  CHECK(ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_PENDING);
  /* First make cancellation genuinely requested. No fixture writes a phase. */
  CHECK(ku_task_driver_request_cancel(&root.ticket,&root.owner.lease,
      KU_TASK_CONTROL_CANCELLED,original)==KU_TASK_CONTROL_OK);
  fixture_idle(driver);
  CHECK(ku_task_control_atomic_load(&parent->control.phase)==KU_TASK_CONTROL_REQUESTED_CANCEL);
  CHECK(ku_task_control_cleanup_deadline(&parent->control)==original);
  CHECK(ku_task_control_cleanup_deadline(&child->control)==original);
  CHECK(!parent->payload_initialized && !parent->drain_failure && !fixture_commit_calls);
  /* Real timer expiry lets actual cleanup choose its timeout result. Pause
   * only at the adapter's existing return OK, after its last propagation. */
  CHECK(!ku_task_driver_lock(driver));
  ku_task_control_deadline_store(&fixture_clock,original);
  (void)ku_task_driver_deadlines_locked(driver,original); ku_task_driver_signal(driver);
  CHECK(!ku_task_driver_unlock(driver));
  CHECK(ku_test_event_wait(&fixture_commit,2000u));
  CHECK(ku_task_control_atomic_load(&parent->control.phase)==KU_TASK_CONTROL_REQUESTED_CANCEL);
  CHECK(ku_task_control_cleanup_deadline(&child->control)==original);
  /* This is a normal concurrent caller while the real worker is held inside
   * cleanup, not a fake deadline store. Successful acceptance is observable. */
  CHECK(ku_task_driver_request_cancel(&root.ticket,&root.owner.lease,
      KU_TASK_CONTROL_CANCELLED,shorter)==KU_TASK_CONTROL_OK);
  CHECK(ku_task_control_cleanup_deadline(&parent->control)==shorter);
  CHECK(ku_task_control_cleanup_deadline(&child->control)==original);
  CHECK(ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_PENDING);
  CHECK(!fixture_input_frees);
  CHECK(ku_test_event_set(&fixture_commit_proceed)); fixture_idle(driver);
  CHECK(ku_task_control_atomic_load(&parent->control.phase)==KU_TASK_CONTROL_CANCELLED);
  CHECK(ku_task_control_cleanup_deadline(&parent->control)==shorter);
  CHECK(ku_task_control_atomic_load(&child->control.phase)==KU_TASK_CONTROL_REQUESTED_CANCEL);
  uint64_t observed_child_deadline=ku_task_control_cleanup_deadline(&child->control);
  CHECK(ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_PENDING);
  CHECK(!fixture_input_frees && fixture_commit_calls>=1u && fixture_commit_calls<=2u);
  /* Complete cleanup before reporting the regression, even on the old path.
   * Late ACK does not retroactively turn the expired cleanup into success. */
  ku_task_control_atomic_store(&fixture_child_hold,0);
  CHECK(ku_task_driver_wake(&child->ticket)==KU_TASK_DRIVER_OK); fixture_idle(driver);
  CHECK(ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_CLEANUP_ACK);
  CHECK(ku_task_control_atomic_load(&child->control.phase)==KU_TASK_CONTROL_CANCELLED);
  CHECK(ku_task_control_cleanup_deadline(&child->control)==observed_child_deadline);
  CHECK(child->control.frame_destroyed && fixture_input_frees==1u);
  CHECK(fixture_cleanup_calls>=2u && fixture_cleanup_calls<=5u);
  CHECK(ku_task_control_atomic_load(&parent->control.phase)==KU_TASK_CONTROL_CANCELLED);
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
  CHECK(ku_test_event_destroy(&fixture_commit));
  CHECK(ku_test_event_destroy(&fixture_commit_proceed));
  CHECK(ku_test_event_destroy(&fixture_exit_event));
  free(ring); free(slots); free(driver);
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes && !fixture_ledger().overflow);
  if (observed_child_deadline!=shorter) {
    fprintf(stderr,"accepted cleanup-commit deadline not propagated: expected=%llu actual=%llu original=%llu\n",
        (unsigned long long)shorter,(unsigned long long)observed_child_deadline,(unsigned long long)original);
  }
  CHECK(observed_child_deadline==shorter);
  puts("task-cleanup-commit-ok"); return 0;
}
"#;
