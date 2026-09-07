//! R5b.1 cleanup receipts over actual generated tasks, not scope-drain/Await.
#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use ku::{
    backend::c,
    checker::Checker,
    ir::{self, task::*, IrType},
    lexer::Lexer,
    parser::Parser,
};
use native_allocation_harness::ALLOCATION_HOOK;
use native_harness::{
    compile_harness, run_bounded, TempDir, NATIVE_THREAD_LIFECYCLE_HARNESS, RUN_LIMITS, RUN_TIMEOUT,
};
use std::{fs, process::Command};

fn fixture_source() -> String {
    let ast = Parser::new(Lexer::new("fn main() {}").lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let sync = ir::lower_program(&ast).unwrap();
    let result = IrType::Result(Box::new(IrType::Str));
    let tasks = TaskProgram {
        functions: vec![TaskFunction {
            id: TaskFunctionId(0),
            name: "ReceiptOwnedFrame".into(),
            slots: [IrType::Int, result.clone()]
                .into_iter()
                .map(|ty| TaskSlot {
                    ty: TaskSlotType::Value {
                        ty,
                        borrowed: false,
                    },
                })
                .collect(),
            parameters: vec![SlotId(0), SlotId(1)],
            entry: StateId(0),
            result,
            states: vec![
                TaskState {
                    operations: vec![],
                    terminator: TaskTerminator::Suspend {
                        resume: StateId(1),
                        cleanup: StateId(2),
                    },
                },
                TaskState {
                    operations: vec![],
                    terminator: TaskTerminator::Complete { value: SlotId(1) },
                },
                TaskState {
                    operations: vec![TaskOp::Drop { slot: SlotId(1) }],
                    terminator: TaskTerminator::Terminate,
                },
            ],
        }],
    };
    c::generate_task_frame_c_source(&sync, &tasks).unwrap()
}

fn replace_once(source: String, anchor: &str, replacement: &str) -> String {
    assert_eq!(source.matches(anchor).count(), 1, "receipt hook: {anchor}");
    source.replacen(anchor, replacement, 1)
}

#[test]
fn native_task_cleanup_receipt_ownership_watermark_and_fault_execute_in_c() {
    let generated = fixture_source();
    for forbidden in ["run_source", "const SOURCE", "task.spawn", "Task.new"] {
        assert!(!generated.contains(forbidden));
    }
    for required in [
        "ku_task_driver_owner_drop_receipt(",
        "ku_task_driver_cleanup_receipt_read(",
        "cleanup_acked_generation",
        "KU_TASK_DRIVER_CLEANUP_ACK",
    ] {
        assert!(
            generated.contains(required),
            "missing receipt ABI: {required}"
        );
    }
    let instrumentation = format!(
        "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\n{LOCKED_ALLOCATIONS}\n{HOOK_DECLARATIONS}\n"
    );
    let source = replace_once(
        generated,
        "typedef struct KuString {",
        &format!("{instrumentation}typedef struct KuString {{"),
    );
    let source = replace_once(
        source,
        "  if (slot->cleanup_fault) return slot->cleanup_fault;\n  KuTaskControlV1* control = slot->driver_lease.control;",
        "  if (slot->cleanup_fault) return slot->cleanup_fault;\n  fixture_before_ack(slot->driver_lease.control);\n  KuTaskControlV1* control = slot->driver_lease.control;",
    );
    let source = replace_once(
        source,
        "static uint64_t ku_task_driver_now_ms(void) {",
        &format!("{CLOCK_HOOK}\nstatic uint64_t fixture_original_now(void) {{"),
    );
    let source = replace_once(
        source,
        "static uint32_t ku_task_0_resume(void* raw) {",
        "static uint32_t ku_task_0_resume(void* raw) {\n  uint32_t observed = fixture_resume(raw);\n  if (observed != UINT32_MAX) return observed;",
    );
    let source = replace_once(
        source,
        "static uint32_t ku_task_0_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {",
        "static uint32_t ku_task_0_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {\n  fixture_cleanup(raw);",
    );
    let source = replace_once(
        source,
        "static void ku_task_0_drop_frame(void* raw) {",
        "static void ku_task_0_drop_frame(void* raw) {\n  fixture_drop_frame(raw);",
    );
    let source = replace_once(
        source,
        "static void ku_task_0_drop_payload(void* raw) {",
        "static void ku_task_0_drop_payload(void* raw) {\n  fixture_drop_payload(raw);",
    );
    let source = replace_once(
        source,
        "static uint32_t ku_task_0_take_payload(void* raw, void* destination) {",
        "static uint32_t ku_task_0_take_payload(void* raw, void* destination) {\n  uint32_t observed = fixture_take(raw);\n  if (observed != UINT32_MAX) return observed;",
    );
    let source = replace_once(
        source,
        "      ? KU_TASK_CONTROL_PAYLOAD_TAKEN : KU_TASK_CONTROL_PAYLOAD_AVAILABLE);\n  return taken;",
        "      ? KU_TASK_CONTROL_PAYLOAD_TAKEN : KU_TASK_CONTROL_PAYLOAD_AVAILABLE);\n  fixture_after_store(control);\n  return taken;",
    );
    let source = replace_once(
        source,
        "  slot->cleanup_acked_generation = slot->generation;\n  return KU_TASK_DRIVER_CLEANUP_ACK;",
        "  slot->cleanup_acked_generation = slot->generation;\n  fixture_ack(slot->driver_lease.control);\n  return KU_TASK_DRIVER_CLEANUP_ACK;",
    );
    let source = replace_once(
        source,
        "static void ku_task_0_dispose(KuTaskControlV1* control, void* raw) {",
        "static void ku_task_0_dispose(KuTaskControlV1* control, void* raw) {\n  void* witness = fixture_before_dispose(raw);",
    );
    let source = replace_once(
        source,
        "    ku_task_adapter_fault(ticket.driver, 0);\n}\nstatic const KuTaskControlOpsV1 ku_task_0_ops",
        "    ku_task_adapter_fault(ticket.driver, 0);\n  fixture_disposed(witness);\n}\nstatic const KuTaskControlOpsV1 ku_task_0_ops",
    );
    let mut source = replace_once(
        source,
        "int main(void) {",
        "static int ku_generated_main(void) {",
    );
    source.push_str(C_MAIN);
    let directory = TempDir::new("task-receipt-v1");
    let c_file = directory.path().join("receipt.c");
    fs::write(&c_file, source).expect("write native receipt fixture");
    let Some(executable) = compile_harness(directory.path(), &c_file, "receipt") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute native receipt fixture"
        );
        return;
    };
    fs::remove_file(c_file).expect("remove source before running native receipt artifact");
    for (fault_case, expected) in [
        (false, "task-receipt-v1-ok\n"),
        (true, "task-receipt-corruption-quarantined-not-drained\n"),
    ] {
        let mut command = Command::new(&executable);
        command.current_dir(directory.path());
        if fault_case {
            command.arg("--corrupt-ack");
        }
        let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS)
            .expect("receipt lifecycle races must remain bounded");
        assert!(
            output.status.success(),
            "receipt fixture failed (corruption={fault_case}):\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().replace('\r', ""),
            expected
        );
        assert!(output.stderr.is_empty());
    }
}

const LEDGER_LOCK: &str = r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr, "receipt check line %d: %s\n", __LINE__, #c); abort(); } } while (0)
#if defined(_WIN32)
static SRWLOCK fixture_allocation_lock = SRWLOCK_INIT;
static void fixture_alloc_lock(void) { AcquireSRWLockExclusive(&fixture_allocation_lock); }
static void fixture_alloc_unlock(void) { ReleaseSRWLockExclusive(&fixture_allocation_lock); }
#else
static pthread_mutex_t fixture_allocation_lock = PTHREAD_MUTEX_INITIALIZER;
static void fixture_alloc_lock(void) { CHECK(!pthread_mutex_lock(&fixture_allocation_lock)); }
static void fixture_alloc_unlock(void) { CHECK(!pthread_mutex_unlock(&fixture_allocation_lock)); }
#endif
"#;

const LOCKED_ALLOCATIONS: &str = r#"
#undef malloc
#undef calloc
#undef realloc
#undef free
static void* fixture_malloc(size_t size) {
  fixture_alloc_lock(); void* result = ku_perf_malloc(size); fixture_alloc_unlock(); return result;
}
static void* fixture_calloc(size_t count, size_t size) {
  fixture_alloc_lock(); void* result = ku_perf_calloc(count, size); fixture_alloc_unlock(); return result;
}
static void* fixture_realloc(void* pointer, size_t size) {
  fixture_alloc_lock(); void* result = ku_perf_realloc(pointer, size); fixture_alloc_unlock(); return result;
}
static void fixture_free(void* pointer) {
  fixture_alloc_lock(); ku_perf_free(pointer); fixture_alloc_unlock();
}
typedef struct FixtureLedger { size_t allocations, bytes, calls; int overflow; } FixtureLedger;
static FixtureLedger fixture_ledger(void) {
  fixture_alloc_lock();
  FixtureLedger result = { ku_perf_live_allocations, ku_perf_live_bytes, ku_perf_calls, ku_perf_overflow };
  fixture_alloc_unlock(); return result;
}
#define malloc fixture_malloc
#define calloc fixture_calloc
#define realloc fixture_realloc
#define free fixture_free
"#;

const HOOK_DECLARATIONS: &str = r#"
static uint32_t fixture_resume(void*);
static void fixture_cleanup(void*);
static void fixture_drop_frame(void*);
static void fixture_drop_payload(void*);
static uint32_t fixture_take(void*);
static void fixture_after_store(void*);
static void fixture_before_ack(void*);
static void fixture_ack(void*);
static void* fixture_before_dispose(void*);
static void fixture_disposed(void*);
"#;

const CLOCK_HOOK: &str = r#"
static KuAtomicRefcount fixture_bad_clock;
static uint64_t fixture_original_now(void);
static uint64_t ku_task_driver_now_ms(void) {
  return ku_task_control_atomic_load(&fixture_bad_clock) ? UINT64_MAX : fixture_original_now();
}
"#;

const C_MAIN: &str = r#"
#define ROLE_COUNT 4u
typedef struct FixtureDriver {
  KuTaskDriverV1* driver;
  KuTaskDriverSlotV1* slots;
  size_t* ring;
  size_t capacity;
} FixtureDriver;
typedef struct FixtureRole {
  KuTaskHandle_0 handle;
  KuTaskDriverTicketV1 ticket;
  KuTaskDriverCleanupReceiptV1 receipt;
  KuTestEvent cleanup_entered, cleanup_proceed, payload_entered, payload_proceed;
  KuTestEvent take_entered, take_proceed, stored, store_proceed, acked, disposed, quarantined;
  KuAtomicRefcount cleanups, frame_drops, payload_drops, takes, acks, disposes;
  int initialized, hold, cleanup_gate, payload_gate, take_gate, take_reject, store_gate, corrupt_ack;
} FixtureRole;
static FixtureRole roles[ROLE_COUNT];
/* An explicit process-lifetime root for the separate corruption subprocess.
 * Reachability is NOT successful cleanup: that subprocess asserts retained
 * refs/bytes and returns normally, with standard sanitizer handlers enabled. */
static FixtureDriver fixture_quarantined_driver;
static uint64_t fixture_deadline(void) { return ku_test_real_now_ms() + 2000u; }
static size_t fixture_count(KuAtomicRefcount* count) { return ku_task_control_atomic_load(count); }
static void fixture_inc(KuAtomicRefcount* count) { ku_task_control_atomic_store(count, fixture_count(count) + 1); }
static FixtureRole* fixture_role(void* raw) {
  KuTaskInstance_0* instance = (KuTaskInstance_0*)raw;
  CHECK(instance->frame.s_0 >= 0 && instance->frame.s_0 < ROLE_COUNT);
  FixtureRole* role = &roles[(size_t)instance->frame.s_0];
  CHECK(role->initialized); return role;
}
static void fixture_role_init(size_t index) {
  CHECK(index < ROLE_COUNT && !roles[index].initialized);
  FixtureRole* role = &roles[index]; memset(role, 0, sizeof(*role));
  CHECK(ku_test_event_init(&role->cleanup_entered)); CHECK(ku_test_event_init(&role->cleanup_proceed));
  CHECK(ku_test_event_init(&role->payload_entered)); CHECK(ku_test_event_init(&role->payload_proceed));
  CHECK(ku_test_event_init(&role->take_entered)); CHECK(ku_test_event_init(&role->take_proceed));
  CHECK(ku_test_event_init(&role->stored)); CHECK(ku_test_event_init(&role->store_proceed));
  CHECK(ku_test_event_init(&role->acked)); CHECK(ku_test_event_init(&role->disposed));
  CHECK(ku_test_event_init(&role->quarantined));
  ku_task_control_atomic_init(&role->cleanups, 0); ku_task_control_atomic_init(&role->frame_drops, 0);
  ku_task_control_atomic_init(&role->payload_drops, 0); ku_task_control_atomic_init(&role->takes, 0);
  ku_task_control_atomic_init(&role->acks, 0); ku_task_control_atomic_init(&role->disposes, 0);
  role->initialized = 1;
}
static void fixture_roles_finish(void) {
  for (size_t i = 0; i < ROLE_COUNT; ++i) {
    FixtureRole* role = &roles[i]; if (!role->initialized) continue;
    CHECK(!role->handle.owner.lease.control);
    CHECK(fixture_count(&role->disposes) == 1u && fixture_count(&role->frame_drops) == 1u);
    CHECK(ku_test_event_destroy(&role->cleanup_entered)); CHECK(ku_test_event_destroy(&role->cleanup_proceed));
    CHECK(ku_test_event_destroy(&role->payload_entered)); CHECK(ku_test_event_destroy(&role->payload_proceed));
    CHECK(ku_test_event_destroy(&role->take_entered)); CHECK(ku_test_event_destroy(&role->take_proceed));
    CHECK(ku_test_event_destroy(&role->stored)); CHECK(ku_test_event_destroy(&role->store_proceed));
    CHECK(ku_test_event_destroy(&role->acked)); CHECK(ku_test_event_destroy(&role->disposed));
    CHECK(ku_test_event_destroy(&role->quarantined));
    memset(role, 0, sizeof(*role));
  }
  FixtureLedger ledger = fixture_ledger();
  CHECK(!ledger.allocations && !ledger.bytes && !ledger.overflow);
}
static KuString fixture_owned(const char* text) {
  size_t length = strlen(text); uint8_t* data = (uint8_t*)malloc(length); CHECK(data);
  memcpy(data, text, length); return (KuString){ data, length, length, KU_STRING_OWNED };
}
static KuResult_str fixture_input(int error) {
  KuResult_str input = {0};
  if (error) {
    input.error.domain = fixture_owned("domain");
    input.error.code = fixture_owned("code");
    input.error.message = fixture_owned("message");
  } else { input.ok = true; input.value = fixture_owned("payload"); }
  return input;
}
static KuTaskDriverSnapshotV1 fixture_snapshot(FixtureDriver* runtime, uint32_t fault) {
  KuTaskDriverSnapshotV1 snapshot = {0};
  CHECK(ku_task_driver_snapshot(runtime->driver, &snapshot) == KU_TASK_DRIVER_OK);
  CHECK(snapshot.fault == fault && snapshot.running <= 1u && snapshot.resident <= runtime->capacity);
  return snapshot;
}
static void fixture_idle(FixtureDriver* runtime) {
  CHECK(ku_task_driver_wait_idle(runtime->driver, fixture_deadline()) == KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot = fixture_snapshot(runtime, 0);
  CHECK(!snapshot.queued && !snapshot.running && (snapshot.worker_waiting || snapshot.worker_exited));
}
static void fixture_driver_init(FixtureDriver* runtime, size_t capacity) {
  memset(runtime, 0, sizeof(*runtime)); runtime->capacity = capacity;
  runtime->driver = (KuTaskDriverV1*)calloc(1, sizeof(*runtime->driver));
  runtime->slots = (KuTaskDriverSlotV1*)calloc(capacity, sizeof(*runtime->slots));
  runtime->ring = (size_t*)calloc(capacity, sizeof(*runtime->ring));
  CHECK(runtime->driver && runtime->slots && runtime->ring);
  size_t fixed = sizeof(*runtime->driver) + capacity * (sizeof(*runtime->slots) + sizeof(*runtime->ring));
  for (uint32_t old_abi = 1; old_abi <= 2; ++old_abi) {
    CHECK(ku_task_driver_init(runtime->driver, sizeof(*runtime->driver), old_abi,
        runtime->slots, capacity, runtime->ring, capacity, fixed + 1048576u) == KU_TASK_DRIVER_ABI_MISMATCH);
    CHECK(ku_task_frame_zero_bytes(runtime->driver, sizeof(*runtime->driver)));
    CHECK(ku_task_frame_zero_bytes(runtime->slots, capacity * sizeof(*runtime->slots)));
    CHECK(ku_task_frame_zero_bytes(runtime->ring, capacity * sizeof(*runtime->ring)));
  }
  CHECK(ku_task_driver_init(runtime->driver, sizeof(*runtime->driver), KU_TASK_DRIVER_ABI_VERSION,
      runtime->slots, capacity, runtime->ring, capacity, fixed + 1048576u) == KU_TASK_DRIVER_OK);
  fixture_idle(runtime); CHECK(fixture_snapshot(runtime, 0).fixed_bytes == fixed);
}
static void fixture_start(FixtureDriver* runtime, size_t index, int error) {
  int64_t id = (int64_t)index; KuResult_str input = fixture_input(error);
  CHECK(ku_task_0_try_start(runtime->driver, &id, &input, &roles[index].handle) == KU_TASK_DRIVER_OK);
  CHECK(!input.value.ptr && !input.error.domain.ptr && !input.error.code.ptr && !input.error.message.ptr);
  roles[index].ticket = roles[index].handle.ticket;
}
static void fixture_transfer(size_t index, uint64_t deadline) {
  FixtureRole* role = &roles[index]; FixtureLedger before = fixture_ledger();
  CHECK(ku_task_driver_owner_drop_receipt(&role->ticket, &role->handle.owner, deadline, &role->receipt) == KU_TASK_DRIVER_OK);
  CHECK(!role->handle.owner.lease.control);
  CHECK(role->receipt.driver == role->ticket.driver && role->receipt.slot == role->ticket.slot);
  CHECK(role->receipt.generation == role->ticket.generation && role->receipt.abi_version == KU_TASK_DRIVER_ABI_VERSION);
  role->handle.ticket = (KuTaskDriverTicketV1){0};
  CHECK(fixture_ledger().calls == before.calls);
}
static void fixture_wait_ack(size_t index) {
  CHECK(ku_test_event_wait(&roles[index].acked, 2000));
  CHECK(ku_task_driver_cleanup_receipt_read(&roles[index].receipt) == KU_TASK_DRIVER_CLEANUP_ACK);
  CHECK(fixture_count(&roles[index].acks) == 1u);
}
static void fixture_wait_disposed(size_t index) {
  CHECK(ku_test_event_wait(&roles[index].disposed, 2000));
  CHECK(fixture_count(&roles[index].disposes) == 1u);
  CHECK(ku_task_driver_cleanup_receipt_read(&roles[index].receipt) == KU_TASK_DRIVER_CLEANUP_ACK);
}
static void fixture_driver_finish(FixtureDriver* runtime, uint32_t expected) {
  CHECK(ku_task_driver_shutdown(runtime->driver, fixture_deadline()) == expected);
  KuTaskDriverSnapshotV1 snapshot = fixture_snapshot(runtime, expected);
  CHECK(!snapshot.resident && !snapshot.reserved_bytes && !snapshot.building && !snapshot.queued && !snapshot.running);
  /* With a sticky broken clock shutdown returns INTERNAL without waiting.
   * All allocations are already disposed; only final worker/OS ACK remains. */
  uint64_t deadline = fixture_deadline(); uint32_t destroyed = KU_TASK_DRIVER_PENDING;
  for (size_t i = 0; i < 4096 && destroyed == KU_TASK_DRIVER_PENDING; ++i) {
    destroyed = ku_task_driver_destroy(runtime->driver);
    if (destroyed == KU_TASK_DRIVER_PENDING) { CHECK(ku_test_real_now_ms() < deadline); ku_test_thread_yield(); }
  }
  CHECK(destroyed == KU_TASK_DRIVER_OK);
  free(runtime->ring); free(runtime->slots); free(runtime->driver);
  memset(runtime, 0, sizeof(*runtime));
}
static void fixture_check_observer(FixtureDriver* runtime, const KuTaskControlLeaseV1* observer,
                                   size_t charge, uint32_t payload, uint32_t fault) {
  KuTaskControlV1* control = observer->control; CHECK(control);
  KuTaskInstance_0* instance = (KuTaskInstance_0*)control->context;
  CHECK(control->frame_destroyed && !ku_task_control_atomic_load(&control->lifecycle_pin));
  CHECK(!instance->frame_initialized && instance->frame.header.status == KU_TASK_FRAME_DESTROYED);
  CHECK(!instance->payload_initialized && ku_task_control_atomic_load(&control->payload) == payload);
  KuTaskDriverSnapshotV1 snapshot = fixture_snapshot(runtime, fault);
  CHECK(snapshot.resident == 1u && snapshot.reserved_bytes == charge && snapshot.retiring == 1u);
  CHECK(fixture_ledger().bytes >= sizeof(*instance));
}
static uint32_t fixture_resume(void* raw) {
  FixtureRole* role = fixture_role(raw);
  if (!role->hold) return UINT32_MAX;
  KuTaskInstance_0* instance = (KuTaskInstance_0*)raw;
  CHECK(ku_task_driver_set_intent(&instance->ticket, KU_TASK_DRIVER_WAIT) == KU_TASK_DRIVER_OK);
  return KU_TASK_CONTROL_PENDING;
}
static void fixture_cleanup(void* raw) {
  FixtureRole* role = fixture_role(raw); fixture_inc(&role->cleanups);
  if (role->cleanup_gate) {
    CHECK(ku_test_event_set(&role->cleanup_entered)); CHECK(ku_test_event_wait(&role->cleanup_proceed, 2000));
  }
}
static void fixture_drop_frame(void* raw) { fixture_inc(&fixture_role(raw)->frame_drops); }
static void fixture_drop_payload(void* raw) {
  FixtureRole* role = fixture_role(raw); fixture_inc(&role->payload_drops);
  if (role->payload_gate) {
    CHECK(ku_test_event_set(&role->payload_entered)); CHECK(ku_test_event_wait(&role->payload_proceed, 2000));
  }
}
static uint32_t fixture_take(void* raw) {
  FixtureRole* role = fixture_role(raw); fixture_inc(&role->takes);
  if (role->take_gate) {
    CHECK(ku_test_event_set(&role->take_entered)); CHECK(ku_test_event_wait(&role->take_proceed, 2000));
    if (role->take_reject) return KU_TASK_CONTROL_INVALID_ARGUMENT;
  }
  return UINT32_MAX;
}
static void fixture_after_store(void* raw) {
  KuTaskControlV1* control = (KuTaskControlV1*)raw;
  FixtureRole* role = fixture_role(control->context);
  if (role->store_gate) {
    CHECK(ku_test_event_set(&role->stored)); CHECK(ku_test_event_wait(&role->store_proceed, 2000));
  }
}
static void fixture_ack(void* raw) {
  KuTaskControlV1* control = (KuTaskControlV1*)raw;
  FixtureRole* role = fixture_role(control->context); fixture_inc(&role->acks);
  CHECK(control->frame_destroyed && !ku_task_control_atomic_load(&control->lifecycle_pin));
  CHECK(ku_test_event_set(&role->acked)); /* Test witness only, no runtime callback or queue. */
}
static void fixture_before_ack(void* raw) {
  KuTaskControlV1* control = (KuTaskControlV1*)raw;
  FixtureRole* role = fixture_role(control->context);
  if (role->corrupt_ack) {
    /* Sole worker, after real R2 cleanup/frame drop/pin release. Deliberately
     * corrupt one guard's metadata; never undo it to manufacture recovery. */
    CHECK(control->frame_destroyed && !ku_task_control_atomic_load(&control->lifecycle_pin));
    control->frame_destroyed = 0;
    CHECK(ku_test_event_set(&role->quarantined));
  }
}
static void* fixture_before_dispose(void* raw) { return fixture_role(raw); }
static void fixture_disposed(void* raw) {
  FixtureRole* role = (FixtureRole*)raw; fixture_inc(&role->disposes);
  CHECK(ku_test_event_set(&role->disposed));
}

static void fixture_cleanup_pending_and_observer(void) {
  FixtureDriver runtime; fixture_driver_init(&runtime, 1); fixture_role_init(0);
  roles[0].hold = 1; roles[0].cleanup_gate = 1;
  fixture_start(&runtime, 0, 0); fixture_idle(&runtime);
  KuTaskControlLeaseV1 observer = {0};
  CHECK(ku_task_control_lease_retain(&roles[0].handle.owner.lease, &observer) == KU_TASK_CONTROL_OK);
  size_t charge = fixture_snapshot(&runtime, 0).reserved_bytes;
  FixtureLedger before = fixture_ledger();
  fixture_transfer(0, fixture_deadline());
  CHECK(ku_test_event_wait(&roles[0].cleanup_entered, 2000));
  CHECK(ku_task_driver_cleanup_receipt_read(&roles[0].receipt) == KU_TASK_DRIVER_PENDING);
  CHECK(!fixture_count(&roles[0].acks) && !fixture_count(&roles[0].frame_drops));
  CHECK(ku_task_control_atomic_load(&observer.control->lifecycle_pin) == 1u);
  CHECK(fixture_ledger().bytes == before.bytes);
  CHECK(ku_test_event_set(&roles[0].cleanup_proceed)); fixture_wait_ack(0); fixture_idle(&runtime);
  fixture_check_observer(&runtime, &observer, charge, KU_TASK_CONTROL_PAYLOAD_EMPTY, 0);
  CHECK(fixture_ledger().bytes == before.bytes - 7u);
  CHECK(ku_task_control_lease_release(&observer) == KU_TASK_CONTROL_OK);
  fixture_wait_disposed(0); fixture_driver_finish(&runtime, 0); fixture_roles_finish();
}
static void fixture_completed_payload_observer(int error) {
  FixtureDriver runtime; fixture_driver_init(&runtime, 1); fixture_role_init(0);
  roles[0].payload_gate = 1; fixture_start(&runtime, 0, error); fixture_idle(&runtime);
  KuTaskControlLeaseV1 observer = {0};
  CHECK(ku_task_control_lease_retain(&roles[0].handle.owner.lease, &observer) == KU_TASK_CONTROL_OK);
  size_t charge = fixture_snapshot(&runtime, 0).reserved_bytes;
  FixtureLedger before = fixture_ledger();
  fixture_transfer(0, fixture_deadline());
  CHECK(ku_test_event_wait(&roles[0].payload_entered, 2000));
  CHECK(ku_task_driver_cleanup_receipt_read(&roles[0].receipt) == KU_TASK_DRIVER_PENDING);
  CHECK(ku_task_control_atomic_load(&observer.control->payload) == KU_TASK_CONTROL_PAYLOAD_DROPPED);
  CHECK(fixture_count(&roles[0].frame_drops) == 1u && !fixture_count(&roles[0].acks));
  CHECK(fixture_ledger().bytes == before.bytes);
  CHECK(ku_test_event_set(&roles[0].payload_proceed)); fixture_wait_ack(0); fixture_idle(&runtime);
  fixture_check_observer(&runtime, &observer, charge, KU_TASK_CONTROL_PAYLOAD_DROPPED, 0);
  CHECK(fixture_ledger().bytes == before.bytes - (error ? 17u : 7u));
  CHECK(fixture_ledger().allocations == before.allocations - (error ? 3u : 1u));
  for (size_t i = 0; i < 8; ++i) CHECK(ku_task_driver_cleanup_receipt_read(&roles[0].receipt) == KU_TASK_DRIVER_CLEANUP_ACK);
  CHECK(fixture_count(&roles[0].payload_drops) == 1u && fixture_count(&roles[0].acks) == 1u);
  CHECK(ku_task_control_lease_release(&observer) == KU_TASK_CONTROL_OK);
  fixture_wait_disposed(0); fixture_driver_finish(&runtime, 0); fixture_roles_finish();
}
static void fixture_preflight(void) {
  FixtureDriver runtime; fixture_driver_init(&runtime, 4); fixture_role_init(0); roles[0].hold = 1;
  fixture_start(&runtime, 0, 0); fixture_idle(&runtime);
  FixtureRole* role = &roles[0]; KuTaskControlV1* control = role->handle.owner.lease.control;
  FixtureLedger before = fixture_ledger(); size_t references = fixture_count(&control->references);
  KuTaskDriverCleanupReceiptV1 nonempty = { runtime.driver, 0, 0, 0 }, original = nonempty;
  CHECK(ku_task_driver_owner_drop_receipt(&role->ticket, &role->handle.owner, 0, NULL) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  uint8_t storage[sizeof(KuTaskDriverCleanupReceiptV1) + KU_TASK_FRAME_ALIGNOF(KuTaskDriverCleanupReceiptV1)];
  uintptr_t aligned = ((uintptr_t)storage + KU_TASK_FRAME_ALIGNOF(KuTaskDriverCleanupReceiptV1) - 1u)
      & ~((uintptr_t)KU_TASK_FRAME_ALIGNOF(KuTaskDriverCleanupReceiptV1) - 1u);
  CHECK(ku_task_driver_owner_drop_receipt(&role->ticket, &role->handle.owner, 0,
      (KuTaskDriverCleanupReceiptV1*)(aligned + 1u)) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(ku_task_driver_owner_drop_receipt(&role->ticket, &role->handle.owner, 0, &nonempty) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(!memcmp(&nonempty, &original, sizeof(original)));
  CHECK(ku_task_driver_owner_drop_receipt(&role->ticket, &role->handle.owner, 0,
      (KuTaskDriverCleanupReceiptV1*)&role->handle.owner) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(ku_task_driver_owner_drop_receipt(&role->ticket, &role->handle.owner, 0,
      (KuTaskDriverCleanupReceiptV1*)&role->ticket) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(ku_task_driver_owner_drop_receipt(&role->ticket, &role->handle.owner, 0,
      (KuTaskDriverCleanupReceiptV1*)control) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(ku_task_driver_owner_drop_receipt(&role->ticket, &role->handle.owner, 0,
      (KuTaskDriverCleanupReceiptV1*)runtime.driver) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(ku_task_driver_owner_drop_receipt(&role->ticket, &role->handle.owner, 0,
      (KuTaskDriverCleanupReceiptV1*)runtime.slots) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(ku_task_driver_owner_drop_receipt(&role->ticket, &role->handle.owner, 0,
      (KuTaskDriverCleanupReceiptV1*)runtime.ring) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(role->handle.owner.lease.control == control && fixture_count(&control->references) == references);
  CHECK(ku_task_control_atomic_load(&control->phase) == KU_TASK_CONTROL_LIVE);
  CHECK(!fixture_count(&role->cleanups) && !fixture_count(&role->acks) && !role->receipt.driver);
  CHECK(fixture_ledger().calls == before.calls && fixture_ledger().bytes == before.bytes);
  fixture_transfer(0, fixture_deadline()); fixture_wait_disposed(0);
  CHECK(ku_task_driver_cleanup_receipt_read(NULL) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  KuTaskDriverCleanupReceiptV1 bad = {0};
  CHECK(ku_task_driver_cleanup_receipt_read(&bad) == KU_TASK_DRIVER_ABI_MISMATCH);
  /* A full receipt-sized header receives the ticket prefix. Never cast a
   * shorter 24-byte ticket and read a nonexistent ABI field beyond it. */
  memcpy(&bad, &role->ticket, sizeof(role->ticket));
  CHECK(ku_task_driver_cleanup_receipt_read(&bad) == KU_TASK_DRIVER_ABI_MISMATCH);
  bad = role->receipt; bad.abi_version = 2;
  CHECK(ku_task_driver_cleanup_receipt_read(&bad) == KU_TASK_DRIVER_ABI_MISMATCH);
  bad = role->receipt; bad.generation = 0;
  CHECK(ku_task_driver_cleanup_receipt_read(&bad) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  bad = role->receipt; bad.generation++;
  CHECK(ku_task_driver_cleanup_receipt_read(&bad) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  bad = role->receipt; bad.slot = runtime.capacity;
  CHECK(ku_task_driver_cleanup_receipt_read(&bad) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(!fixture_snapshot(&runtime, 0).resident);
  fixture_driver_finish(&runtime, 0); fixture_roles_finish();
}
static void fixture_dispose_reuse_and_rollback(void) {
  FixtureDriver runtime; fixture_driver_init(&runtime, 1); fixture_role_init(0);
  fixture_start(&runtime, 0, 0); fixture_idle(&runtime);
  fixture_transfer(0, fixture_deadline()); fixture_wait_disposed(0); fixture_idle(&runtime);
  uint64_t watermark = roles[0].receipt.generation;
  CHECK(!fixture_snapshot(&runtime, 0).resident);
  FixtureLedger base = fixture_ledger();
  for (size_t i = 0; i < 8; ++i) {
    KuTaskDriverTicketV1 building = {0};
    CHECK(ku_task_driver_reserve(runtime.driver, 13u, &building) == KU_TASK_DRIVER_OK);
    CHECK(building.generation > watermark);
    CHECK(ku_task_driver_cleanup_receipt_read(&roles[0].receipt) == KU_TASK_DRIVER_CLEANUP_ACK);
    CHECK(!ku_task_driver_lock(runtime.driver));
    CHECK(runtime.slots[0].cleanup_acked_generation == watermark);
    CHECK(!ku_task_driver_unlock(runtime.driver));
    CHECK(ku_task_driver_rollback(&building) == KU_TASK_DRIVER_OK && !building.driver);
    KuTaskDriverSnapshotV1 snapshot = fixture_snapshot(&runtime, 0);
    CHECK(!snapshot.resident && !snapshot.building && !snapshot.reserved_bytes);
    CHECK(fixture_ledger().calls == base.calls && fixture_ledger().bytes == base.bytes);
  }
  fixture_role_init(1); roles[1].hold = 1; fixture_start(&runtime, 1, 1); fixture_idle(&runtime);
  CHECK(roles[1].ticket.slot == roles[0].receipt.slot && roles[1].ticket.generation > watermark);
  KuTaskControlV1* replacement = roles[1].handle.owner.lease.control;
  size_t refs = fixture_count(&replacement->references); FixtureLedger held = fixture_ledger();
  for (size_t i = 0; i < 8; ++i) CHECK(ku_task_driver_cleanup_receipt_read(&roles[0].receipt) == KU_TASK_DRIVER_CLEANUP_ACK);
  CHECK(fixture_count(&replacement->references) == refs && ku_task_control_atomic_load(&replacement->phase) == KU_TASK_CONTROL_LIVE);
  CHECK(fixture_ledger().bytes == held.bytes && fixture_ledger().calls == held.calls);
  fixture_transfer(1, fixture_deadline()); fixture_wait_disposed(1);
  CHECK(ku_task_driver_cleanup_receipt_read(&roles[0].receipt) == KU_TASK_DRIVER_CLEANUP_ACK);
  fixture_driver_finish(&runtime, 0); fixture_roles_finish();
}
typedef struct FixtureTake {
  KuTaskDriverTicketV1 ticket;
  KuTaskControlLeaseV1 lease;
  KuResult_str output;
  uint32_t status;
} FixtureTake;
static int fixture_take_thread(void* raw) {
  FixtureTake* take = (FixtureTake*)raw;
  take->status = ku_task_driver_take_result(&take->ticket, &take->lease, &take->output); return 0;
}
static void fixture_taking(int reject) {
  FixtureDriver runtime; fixture_driver_init(&runtime, 1); fixture_role_init(0);
  roles[0].take_gate = 1; roles[0].take_reject = reject; roles[0].store_gate = 1;
  fixture_start(&runtime, 0, 0); fixture_idle(&runtime);
  KuTaskControlLeaseV1 observer = {0}; FixtureTake take = {0}; take.ticket = roles[0].ticket;
  CHECK(ku_task_control_lease_retain(&roles[0].handle.owner.lease, &observer) == KU_TASK_CONTROL_OK);
  CHECK(ku_task_control_lease_retain(&roles[0].handle.owner.lease, &take.lease) == KU_TASK_CONTROL_OK);
  size_t charge = fixture_snapshot(&runtime, 0).reserved_bytes;
  KuTestThread thread; CHECK(ku_test_thread_start(&thread, fixture_take_thread, &take));
  CHECK(ku_test_event_wait(&roles[0].take_entered, 2000));
  fixture_transfer(0, fixture_deadline()); fixture_idle(&runtime);
  CHECK(ku_task_driver_cleanup_receipt_read(&roles[0].receipt) == KU_TASK_DRIVER_PENDING);
  CHECK(ku_task_control_atomic_load(&observer.control->payload) == KU_TASK_CONTROL_PAYLOAD_TAKING);
  CHECK(!fixture_count(&roles[0].acks));
  CHECK(ku_test_event_set(&roles[0].take_proceed)); CHECK(ku_test_event_wait(&roles[0].stored, 2000));
  CHECK(ku_task_driver_cleanup_receipt_read(&roles[0].receipt) == KU_TASK_DRIVER_PENDING);
  CHECK(!fixture_count(&roles[0].acks));
  CHECK(ku_test_event_set(&roles[0].store_proceed));
  CHECK(ku_test_thread_join(&thread, 2000) && thread.outcome == 0);
  CHECK(take.status == (reject ? KU_TASK_CONTROL_INVALID_ARGUMENT : KU_TASK_CONTROL_OK));
  fixture_wait_ack(0); fixture_idle(&runtime);
  fixture_check_observer(&runtime, &observer, charge,
      reject ? KU_TASK_CONTROL_PAYLOAD_DROPPED : KU_TASK_CONTROL_PAYLOAD_TAKEN, 0);
  CHECK(fixture_count(&roles[0].payload_drops) == (reject ? 1u : 0u));
  CHECK(ku_task_control_lease_release(&take.lease) == KU_TASK_CONTROL_OK);
  ku_result_drop_str(&take.output);
  CHECK(!fixture_count(&roles[0].disposes));
  CHECK(ku_task_control_lease_release(&observer) == KU_TASK_CONTROL_OK);
  fixture_wait_disposed(0); fixture_driver_finish(&runtime, 0); fixture_roles_finish();
}
static void fixture_reference_limit(void) {
  FixtureDriver runtime; fixture_driver_init(&runtime, 1); fixture_role_init(0);
  roles[0].hold = 1; roles[0].cleanup_gate = 1; fixture_start(&runtime, 0, 0); fixture_idle(&runtime);
  KuTaskControlV1* control = roles[0].handle.owner.lease.control;
  size_t count = KU_TASK_CONTROL_MAX_REFERENCES - fixture_count(&control->references);
  KuTaskControlLeaseV1* leases = (KuTaskControlLeaseV1*)calloc(count, sizeof(*leases)); CHECK(leases && count);
  for (size_t i = 0; i < count; ++i)
    CHECK(ku_task_control_lease_retain(&roles[0].handle.owner.lease, &leases[i]) == KU_TASK_CONTROL_OK);
  CHECK(fixture_count(&control->references) == KU_TASK_CONTROL_MAX_REFERENCES);
  size_t charge = fixture_snapshot(&runtime, 0).reserved_bytes;
  fixture_transfer(0, fixture_deadline()); CHECK(ku_test_event_wait(&roles[0].cleanup_entered, 2000));
  CHECK(ku_task_driver_cleanup_receipt_read(&roles[0].receipt) == KU_TASK_DRIVER_PENDING);
  CHECK(fixture_count(&control->references) == KU_TASK_CONTROL_MAX_REFERENCES - 1u);
  CHECK(ku_test_event_set(&roles[0].cleanup_proceed)); fixture_wait_ack(0); fixture_idle(&runtime);
  fixture_check_observer(&runtime, &leases[0], charge, KU_TASK_CONTROL_PAYLOAD_EMPTY, 0);
  CHECK(fixture_count(&control->references) == count); /* Receipt retained nothing. */
  for (size_t i = 0; i < count; ++i) CHECK(ku_task_control_lease_release(&leases[i]) == KU_TASK_CONTROL_OK);
  free(leases); fixture_wait_disposed(0); fixture_driver_finish(&runtime, 0); fixture_roles_finish();
}
static void fixture_generation_limit(void) {
  FixtureDriver runtime; fixture_driver_init(&runtime, 1);
  CHECK(!ku_task_driver_lock(runtime.driver));
  CHECK(runtime.slots[0].state == KU_TASK_DRIVER_FREE && !runtime.slots[0].cleanup_acked_generation);
  runtime.slots[0].generation = UINT64_MAX - 2u; /* Bounded counter-range injection only. */
  CHECK(!ku_task_driver_unlock(runtime.driver));
  KuTaskDriverTicketV1 building = {0};
  CHECK(ku_task_driver_reserve(runtime.driver, 1u, &building) == KU_TASK_DRIVER_OK);
  CHECK(building.generation == UINT64_MAX - 1u);
  CHECK(ku_task_driver_rollback(&building) == KU_TASK_DRIVER_OK);
  CHECK(!ku_task_driver_lock(runtime.driver)); CHECK(!runtime.slots[0].cleanup_acked_generation);
  CHECK(!ku_task_driver_unlock(runtime.driver));
  fixture_role_init(0); fixture_start(&runtime, 0, 0); fixture_idle(&runtime);
  CHECK(roles[0].ticket.generation == UINT64_MAX);
  fixture_transfer(0, fixture_deadline()); fixture_wait_disposed(0); fixture_idle(&runtime);
  CHECK(!ku_task_driver_lock(runtime.driver));
  CHECK(runtime.slots[0].generation == UINT64_MAX && runtime.slots[0].cleanup_acked_generation == UINT64_MAX);
  CHECK(!ku_task_driver_unlock(runtime.driver));
  CHECK(ku_task_driver_reserve(runtime.driver, 1u, &building) == KU_TASK_DRIVER_LIMIT && !building.driver);
  CHECK(ku_task_driver_cleanup_receipt_read(&roles[0].receipt) == KU_TASK_DRIVER_CLEANUP_ACK);
  fixture_driver_finish(&runtime, 0); fixture_roles_finish();
}
static void fixture_close_and_clock_fault(int bad_clock) {
  FixtureDriver runtime; fixture_driver_init(&runtime, 1); fixture_role_init(0);
  roles[0].hold = 1; fixture_start(&runtime, 0, 0); fixture_idle(&runtime);
  KuTaskControlLeaseV1 observer = {0};
  CHECK(ku_task_control_lease_retain(&roles[0].handle.owner.lease, &observer) == KU_TASK_CONTROL_OK);
  size_t charge = fixture_snapshot(&runtime, 0).reserved_bytes;
  if (bad_clock) ku_task_control_atomic_store(&fixture_bad_clock, 1);
  uint32_t stopped = ku_task_driver_shutdown(runtime.driver, 0);
  CHECK(stopped == (bad_clock ? KU_TASK_DRIVER_INTERNAL : KU_TASK_DRIVER_SHUTDOWN_TIMEOUT));
  fixture_transfer(0, 0); fixture_wait_ack(0);
  uint32_t fault = bad_clock ? KU_TASK_DRIVER_INTERNAL : 0;
  if (!bad_clock) fixture_idle(&runtime);
  fixture_check_observer(&runtime, &observer, charge, KU_TASK_CONTROL_PAYLOAD_EMPTY, fault);
  CHECK(ku_task_control_lease_release(&observer) == KU_TASK_CONTROL_OK);
  fixture_wait_disposed(0);
  if (!bad_clock) fixture_idle(&runtime); /* Final worker ACK, not a renewed cleanup budget. */
  fixture_driver_finish(&runtime, fault);
  /* Never clear a live driver's sticky clock fault to manufacture success. */
  if (bad_clock) ku_task_control_atomic_store(&fixture_bad_clock, 0);
  fixture_roles_finish();
}
static void fixture_corrupt_ack_process(void) {
  FixtureDriver* runtime = &fixture_quarantined_driver;
  fixture_driver_init(runtime, 1); fixture_role_init(0); roles[0].corrupt_ack = 1;
  fixture_start(runtime, 0, 0); fixture_idle(runtime);
  FixtureLedger before = fixture_ledger();
  size_t charge = fixture_snapshot(runtime, 0).reserved_bytes;
  fixture_transfer(0, fixture_deadline());
  CHECK(ku_test_event_wait(&roles[0].quarantined, 2000));
  CHECK(ku_task_driver_wait_idle(runtime->driver, fixture_deadline()) == KU_TASK_DRIVER_INTERNAL);
  CHECK(ku_task_driver_cleanup_receipt_read(&roles[0].receipt) == KU_TASK_DRIVER_INTERNAL);
  KuTaskDriverSnapshotV1 snapshot = fixture_snapshot(runtime, KU_TASK_DRIVER_INTERNAL);
  CHECK(snapshot.resident == 1u && snapshot.reserved_bytes == charge && !snapshot.queued && !snapshot.running);
  CHECK(snapshot.terminal_held == 1u && snapshot.worker_waiting);
  CHECK(!ku_task_driver_lock(runtime->driver));
  KuTaskDriverSlotV1* slot = &runtime->slots[0];
  CHECK(slot->cleanup_fault == KU_TASK_DRIVER_INTERNAL && !slot->cleanup_acked_generation);
  CHECK(slot->owner_location == KU_TASK_DRIVER_OWNER_RELEASED && !slot->deferred_owner.lease.control);
  CHECK(slot->driver_lease.control && slot->execution_lease.control && slot->binding);
  CHECK(fixture_count(&slot->binding->references) == 2u && !fixture_count(&slot->binding->lifecycle_pin));
  size_t resident = runtime->driver->resident, reserved = runtime->driver->reserved_bytes;
  CHECK(ku_task_driver_return_slot(runtime->driver, slot) == KU_TASK_DRIVER_INTERNAL);
  CHECK(runtime->driver->resident == resident && runtime->driver->reserved_bytes == reserved);
  CHECK(slot->driver_lease.control && slot->execution_lease.control && slot->binding);
  CHECK(!ku_task_driver_unlock(runtime->driver));
  CHECK(fixture_count(&roles[0].frame_drops) == 1u && fixture_count(&roles[0].payload_drops) == 1u);
  CHECK(!fixture_count(&roles[0].acks) && !fixture_count(&roles[0].disposes));
  CHECK(fixture_ledger().bytes == before.bytes - 7u && fixture_ledger().allocations == 4u);
  for (size_t i = 0; i < 8; ++i) {
    CHECK(ku_task_driver_wake(&roles[0].ticket) == KU_TASK_DRIVER_INTERNAL);
    CHECK(ku_task_driver_cleanup_receipt_read(&roles[0].receipt) == KU_TASK_DRIVER_INTERNAL);
    CHECK(ku_task_driver_wait_idle(runtime->driver, fixture_deadline()) == KU_TASK_DRIVER_INTERNAL);
    CHECK(fixture_snapshot(runtime, KU_TASK_DRIVER_INTERNAL).polls == snapshot.polls);
  }
  CHECK(ku_task_driver_shutdown(runtime->driver, 0) == KU_TASK_DRIVER_SHUTDOWN_TIMEOUT);
  CHECK(ku_task_driver_destroy(runtime->driver) == KU_TASK_DRIVER_PENDING);
  CHECK(fixture_snapshot(runtime, KU_TASK_DRIVER_INTERNAL).reserved_bytes == charge);
  CHECK(fixture_ledger().allocations == 4u && fixture_ledger().bytes != 0u);
  /* Do not clear fault/refs, manually free the task, disable LSan, or use
   * _Exit. The explicit live root remains until normal process termination;
   * OS reclamation is intentionally not counted as native cleanup success. */
  puts("task-receipt-corruption-quarantined-not-drained");
}
int main(int argc, char** argv) {
  CHECK(KU_TASK_DRIVER_ABI_VERSION == 3u);
  ku_task_control_atomic_init(&fixture_bad_clock, 0);
  if (argc == 2 && !strcmp(argv[1], "--corrupt-ack")) { fixture_corrupt_ack_process(); return 0; }
  CHECK(argc == 1);
  fixture_cleanup_pending_and_observer();
  fixture_completed_payload_observer(0); fixture_completed_payload_observer(1);
  fixture_preflight(); fixture_dispose_reuse_and_rollback();
  fixture_taking(0); fixture_taking(1); fixture_reference_limit(); fixture_generation_limit();
  fixture_close_and_clock_fault(0); fixture_close_and_clock_fault(1);
  puts("task-receipt-v1-ok"); return 0;
}
"#;
