//! R5a result-ready registration over real generated R4 tasks and the R3 worker.
//! This is not source Await, scope cleanup ACK, netpoll, or a performance claim.

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
            name: "WaitOwnedFrame".into(),
            slots: [IrType::Int, IrType::Str, result.clone()]
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
                    operations: vec![TaskOp::WrapOk {
                        dst: SlotId(2),
                        src: SlotId(1),
                    }],
                    terminator: TaskTerminator::Complete { value: SlotId(2) },
                },
                TaskState {
                    operations: vec![
                        TaskOp::DropIfInit { slot: SlotId(2) },
                        TaskOp::DropIfInit { slot: SlotId(1) },
                    ],
                    terminator: TaskTerminator::Terminate,
                },
            ],
        }],
    };
    c::generate_task_frame_c_source(&sync, &tasks).unwrap()
}

fn replace_once(source: String, anchor: &str, replacement: &str) -> String {
    assert_eq!(source.matches(anchor).count(), 1, "fixture hook: {anchor}");
    source.replacen(anchor, replacement, 1)
}

#[test]
fn native_task_wait_registration_publication_cancel_and_epoch_execute_in_c() {
    let generated = fixture_source();
    for forbidden in ["run_source", "const SOURCE", "task.spawn", "Task.new"] {
        assert!(!generated.contains(forbidden));
    }
    for required in [
        "ku_task_driver_wait_arm(",
        "ku_task_driver_wait_read(",
        "ku_task_driver_wait_detach(",
        "KuTaskDriverWaitTokenV1",
        "KU_TASK_DRIVER_WAIT_ABORTED",
    ] {
        assert!(
            generated.contains(required),
            "missing native wait: {required}"
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
        "static uint32_t ku_task_0_resume(void* raw) {",
        "static uint32_t ku_task_0_resume(void* raw) {\n  uint32_t fixture_outcome = fixture_resume_hook(raw);\n  if (fixture_outcome != UINT32_MAX) return fixture_outcome;",
    );
    let source = replace_once(
        source,
        "static uint32_t ku_task_0_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {",
        "static uint32_t ku_task_0_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {\n  fixture_cleanup_hook(raw);",
    );
    let source = replace_once(
        source,
        "static uint32_t ku_task_0_take_payload(void* raw, void* destination) {",
        "static uint32_t ku_task_0_take_payload(void* raw, void* destination) {\n  uint32_t fixture_outcome = fixture_take_hook(raw, destination);\n  if (fixture_outcome != UINT32_MAX) return fixture_outcome;",
    );
    let source = replace_once(
        source,
        "      ? KU_TASK_CONTROL_PAYLOAD_TAKEN : KU_TASK_CONTROL_PAYLOAD_AVAILABLE);\n  return taken;",
        "      ? KU_TASK_CONTROL_PAYLOAD_TAKEN : KU_TASK_CONTROL_PAYLOAD_AVAILABLE);\n  fixture_after_take_store(control);\n  return taken;",
    );
    let mut source = replace_once(
        source,
        "int main(void) {",
        "static int ku_generated_main(void) {",
    );
    source.push_str(C_MAIN);
    let directory = TempDir::new("task-wait-v1");
    let c_file = directory.path().join("wait.c");
    fs::write(&c_file, source).expect("write generated wait fixture");
    let Some(executable) = compile_harness(directory.path(), &c_file, "wait") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute native wait registration"
        );
        return;
    };
    fs::remove_file(c_file).expect("remove C source before executing native artifact");
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("native wait races must stay bounded");
    assert!(
        output.status.success(),
        "native wait fixture failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-wait-v1-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const LEDGER_LOCK: &str = r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr, "wait check line %d: %s\n", __LINE__, #c); abort(); } } while (0)
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
  fixture_alloc_lock(); void* output = ku_perf_malloc(size); fixture_alloc_unlock(); return output;
}
static void* fixture_calloc(size_t count, size_t size) {
  fixture_alloc_lock(); void* output = ku_perf_calloc(count, size); fixture_alloc_unlock(); return output;
}
static void* fixture_realloc(void* value, size_t size) {
  fixture_alloc_lock(); void* output = ku_perf_realloc(value, size); fixture_alloc_unlock(); return output;
}
static void fixture_free(void* value) {
  fixture_alloc_lock(); ku_perf_free(value); fixture_alloc_unlock();
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
static uint32_t fixture_resume_hook(void* raw);
static void fixture_cleanup_hook(void* raw);
static uint32_t fixture_take_hook(void* raw, void* destination);
static void fixture_after_take_store(void* raw);
"#;

const C_MAIN: &str = r#"
#define ROLES 12u
enum { MODE_FINISH, MODE_HOLD, MODE_WAIT, MODE_PROBE };
enum { PROBE_EPOCH, PROBE_SELF, PROBE_OCCUPIED, PROBE_CROSS, PROBE_HEADERS, PROBE_DAMAGED_PAIR };
typedef struct FixtureDriver {
  KuTaskDriverV1* driver;
  KuTaskDriverSlotV1* slots;
  size_t* ring;
  size_t capacity;
} FixtureDriver;
typedef struct FixtureRole {
  KuTaskHandle_0 handle;
  KuTaskDriverWaitTokenV1 wait, old_wait;
  KuResult_str result;
  KuTestEvent entered, proceed, armed, cleanup_entered, cleanup_proceed;
  KuTestEvent take_entered, take_proceed, stored, store_proceed;
  KuAtomicRefcount resumes, normal_continuations, cleanups, take_calls;
  size_t target;
  uint32_t mode, probe, expected_arm, arm_status, take_status;
  int initialized, entry_gate, after_arm_gate, cleanup_gate, take_gate, take_reject, store_gate;
  int take_child;
} FixtureRole;
static FixtureRole fixture_roles[ROLES];
static uint64_t fixture_deadline(void) { return ku_task_driver_now_ms() + 2000u; }
static size_t fixture_count(KuAtomicRefcount* value) { return ku_task_control_atomic_load(value); }
static void fixture_inc(KuAtomicRefcount* value) { ku_task_control_atomic_store(value, fixture_count(value) + 1); }
static FixtureRole* fixture_role(void* raw) {
  KuTaskInstance_0* instance = (KuTaskInstance_0*)raw;
  /* Copy role metadata remains in the typed frame allocation after drop.
   * No owned payload or destroyed frame operation is accessed here. */
  CHECK(instance->frame.s_0 >= 0 && instance->frame.s_0 < ROLES);
  FixtureRole* role = &fixture_roles[(size_t)instance->frame.s_0];
  CHECK(role->initialized); return role;
}
static void fixture_role_init(size_t index, uint32_t mode) {
  CHECK(index < ROLES && !fixture_roles[index].initialized);
  FixtureRole* role = &fixture_roles[index];
  memset(role, 0, sizeof(*role)); role->mode = mode; role->target = ROLES;
  role->expected_arm = KU_TASK_DRIVER_PENDING;
  CHECK(ku_test_event_init(&role->entered)); CHECK(ku_test_event_init(&role->proceed));
  CHECK(ku_test_event_init(&role->armed)); CHECK(ku_test_event_init(&role->cleanup_entered));
  CHECK(ku_test_event_init(&role->cleanup_proceed)); CHECK(ku_test_event_init(&role->take_entered));
  CHECK(ku_test_event_init(&role->take_proceed)); CHECK(ku_test_event_init(&role->stored));
  CHECK(ku_test_event_init(&role->store_proceed));
  ku_task_control_atomic_init(&role->resumes, 0); ku_task_control_atomic_init(&role->normal_continuations, 0);
  ku_task_control_atomic_init(&role->cleanups, 0); ku_task_control_atomic_init(&role->take_calls, 0);
  role->initialized = 1;
}
static void fixture_roles_finish(void) {
  for (size_t i = 0; i < ROLES; ++i) {
    FixtureRole* role = &fixture_roles[i]; if (!role->initialized) continue;
    CHECK(!role->handle.owner.lease.control);
    ku_result_drop_str(&role->result);
    CHECK(ku_test_event_destroy(&role->entered)); CHECK(ku_test_event_destroy(&role->proceed));
    CHECK(ku_test_event_destroy(&role->armed)); CHECK(ku_test_event_destroy(&role->cleanup_entered));
    CHECK(ku_test_event_destroy(&role->cleanup_proceed)); CHECK(ku_test_event_destroy(&role->take_entered));
    CHECK(ku_test_event_destroy(&role->take_proceed)); CHECK(ku_test_event_destroy(&role->stored));
    CHECK(ku_test_event_destroy(&role->store_proceed)); memset(role, 0, sizeof(*role));
  }
}
static KuString fixture_owned(void) {
  static const uint8_t bytes[] = { 'w', 'a', 'i', 't', 0, 0xe4, 0xb8, 0xad };
  uint8_t* data = (uint8_t*)malloc(sizeof(bytes)); CHECK(data);
  memcpy(data, bytes, sizeof(bytes));
  return (KuString){ data, sizeof(bytes), sizeof(bytes), KU_STRING_OWNED };
}
static KuTaskDriverSnapshotV1 fixture_snapshot(FixtureDriver* runtime) {
  KuTaskDriverSnapshotV1 snapshot = {0};
  CHECK(ku_task_driver_snapshot(runtime->driver, &snapshot) == KU_TASK_DRIVER_OK);
  CHECK(!snapshot.fault && snapshot.running <= 1 && snapshot.queued <= runtime->capacity);
  CHECK(snapshot.resident <= runtime->capacity); return snapshot;
}
static void fixture_idle(FixtureDriver* runtime) {
  CHECK(ku_task_driver_wait_idle(runtime->driver, fixture_deadline()) == KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot = fixture_snapshot(runtime);
  CHECK(!snapshot.queued && !snapshot.running && (snapshot.worker_waiting || snapshot.worker_exited));
}
static void fixture_driver_init(FixtureDriver* runtime, size_t capacity) {
  memset(runtime, 0, sizeof(*runtime)); runtime->capacity = capacity;
  runtime->driver = (KuTaskDriverV1*)calloc(1, sizeof(*runtime->driver));
  runtime->slots = (KuTaskDriverSlotV1*)calloc(capacity, sizeof(*runtime->slots));
  runtime->ring = (size_t*)calloc(capacity, sizeof(*runtime->ring));
  CHECK(runtime->driver && runtime->slots && runtime->ring);
  size_t fixed = sizeof(*runtime->driver) + capacity * (sizeof(*runtime->slots) + sizeof(*runtime->ring));
  CHECK(ku_task_driver_init(runtime->driver, sizeof(*runtime->driver), 1u,
      runtime->slots, capacity, runtime->ring, capacity, fixed + 1024u * 1024u) == KU_TASK_DRIVER_ABI_MISMATCH);
  CHECK(ku_task_frame_zero_bytes(runtime->driver, sizeof(*runtime->driver)));
  CHECK(ku_task_frame_zero_bytes(runtime->slots, capacity * sizeof(*runtime->slots)));
  CHECK(ku_task_frame_zero_bytes(runtime->ring, capacity * sizeof(*runtime->ring)));
  CHECK(ku_task_driver_init(runtime->driver, sizeof(*runtime->driver), KU_TASK_DRIVER_ABI_VERSION,
      runtime->slots, capacity, runtime->ring, capacity, fixed + 1024u * 1024u) == KU_TASK_DRIVER_OK);
  fixture_idle(runtime);
  CHECK(fixture_snapshot(runtime).fixed_bytes == fixed);
}
static void fixture_start(FixtureDriver* runtime, size_t index) {
  int64_t id = (int64_t)index; KuString payload = fixture_owned();
  CHECK(ku_task_0_try_start(runtime->driver, &id, &payload, &fixture_roles[index].handle) == KU_TASK_DRIVER_OK);
  CHECK(!payload.ptr && id == (int64_t)index);
}
static void fixture_drop(size_t index) {
  FixtureRole* role = &fixture_roles[index];
  if (role->handle.owner.lease.control) CHECK(ku_task_0_drop(&role->handle, fixture_deadline()) == KU_TASK_DRIVER_OK);
}
static void fixture_driver_finish_expected(FixtureDriver* runtime, uint32_t expected) {
  CHECK(ku_task_driver_shutdown(runtime->driver, fixture_deadline()) == expected);
  KuTaskDriverSnapshotV1 snapshot = {0};
  CHECK(ku_task_driver_snapshot(runtime->driver, &snapshot) == KU_TASK_DRIVER_OK);
  CHECK(snapshot.fault == expected);
  CHECK(!snapshot.resident && !snapshot.reserved_bytes && !snapshot.queued && !snapshot.running);
  CHECK(snapshot.worker_exited);
  uint32_t status = KU_TASK_DRIVER_PENDING; uint64_t deadline = fixture_deadline();
  /* Only final Windows OS-thread acknowledgement may lag worker_exited. */
  for (size_t i = 0; i < 4096 && status == KU_TASK_DRIVER_PENDING; ++i) {
    status = ku_task_driver_destroy(runtime->driver);
    if (status == KU_TASK_DRIVER_PENDING) { CHECK(ku_task_driver_now_ms() < deadline); ku_test_thread_yield(); }
  }
  CHECK(status == KU_TASK_DRIVER_OK);
  free(runtime->ring); free(runtime->slots); free(runtime->driver); memset(runtime, 0, sizeof(*runtime));
}
static void fixture_driver_finish(FixtureDriver* runtime) {
  fixture_driver_finish_expected(runtime, KU_TASK_DRIVER_OK);
}
static void fixture_zero(void) {
  fixture_roles_finish(); FixtureLedger ledger = fixture_ledger();
  CHECK(!ledger.allocations && !ledger.bytes && !ledger.overflow);
}
static KuTaskDriverWaitSnapshotV1 fixture_wait_snapshot(FixtureRole* role) {
  KuTaskDriverWaitSnapshotV1 snapshot = {0};
  CHECK(ku_task_driver_wait_read(&role->wait, &snapshot) == KU_TASK_DRIVER_OK); return snapshot;
}
static void fixture_assert_armed(FixtureRole* role) {
  KuTaskDriverWaitSnapshotV1 snapshot = fixture_wait_snapshot(role);
  CHECK(snapshot.state == 1u && snapshot.outcome == KU_TASK_DRIVER_PENDING);
  CHECK(snapshot.child_slot == fixture_roles[role->target].handle.ticket.slot);
  CHECK(snapshot.child_generation == fixture_roles[role->target].handle.ticket.generation);
}

static void fixture_header_probe(FixtureRole* role, KuTaskInstance_0* instance, FixtureRole* child) {
  FixtureLedger before = fixture_ledger();
  KuTaskDriverTicketV1 parent_ticket = instance->ticket, child_ticket = child->handle.ticket;
  KuTaskControlLeaseV1 child_lease = child->handle.owner.lease;
  /* Only complete existing headers or explicitly known shorter aliases are
   * used. No test dereferences an unmapped/forged arbitrary address. */
  CHECK(ku_task_driver_wait_arm(&instance->ticket, &child->handle.ticket, &child->handle.owner.lease,
      (KuTaskDriverWaitTokenV1*)&instance->ticket) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(ku_task_driver_wait_arm(&instance->ticket, &child->handle.ticket, &child->handle.owner.lease,
      (KuTaskDriverWaitTokenV1*)&child->handle.ticket) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(ku_task_driver_wait_arm(&instance->ticket, &child->handle.ticket, &child->handle.owner.lease,
      (KuTaskDriverWaitTokenV1*)&child->handle.owner.lease) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(ku_task_driver_wait_arm(&instance->ticket, &child->handle.ticket, &child->handle.owner.lease,
      (KuTaskDriverWaitTokenV1*)&instance->control) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(ku_task_driver_wait_arm(&instance->ticket, &child->handle.ticket, &child->handle.owner.lease,
      (KuTaskDriverWaitTokenV1*)child->handle.owner.lease.control) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(!memcmp(&parent_ticket, &instance->ticket, sizeof(parent_ticket)));
  CHECK(!memcmp(&child_ticket, &child->handle.ticket, sizeof(child_ticket)));
  CHECK(child_lease.control == child->handle.owner.lease.control);
  CHECK(ku_task_driver_wait_arm(&instance->ticket, &child->handle.ticket,
      &child->handle.owner.lease, &role->wait) == KU_TASK_DRIVER_PENDING);
  KuTaskDriverWaitTokenV1 original = role->wait;
  CHECK(ku_task_driver_wait_read(&role->wait, (KuTaskDriverWaitSnapshotV1*)&role->wait) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(ku_task_driver_wait_read(&role->wait, (KuTaskDriverWaitSnapshotV1*)&instance->control) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(ku_task_driver_wait_read(&role->wait, (KuTaskDriverWaitSnapshotV1*)child->handle.owner.lease.control) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(!memcmp(&original, &role->wait, sizeof(original)));

  /* Runtime-owned storage is rejected too; there are no queued ring entries
   * while this sole worker executes the probe and its child is PARKED. The
   * allocation is temporarily populated with a complete token via memcpy;
   * no typed control/callback metadata is reinterpreted as a token. */
  CHECK(!ku_task_driver_lock(instance->ticket.driver));
  CHECK(instance->ticket.driver->queued == 0u);
  CHECK(instance->ticket.driver->capacity * sizeof(size_t) >= sizeof(original));
  uint8_t saved_ring[sizeof(KuTaskDriverWaitTokenV1)];
  memcpy(saved_ring, instance->ticket.driver->ring, sizeof(saved_ring));
  memcpy(instance->ticket.driver->ring, &original, sizeof(original));
  CHECK(!ku_task_driver_unlock(instance->ticket.driver));
  uint32_t alias_status = ku_task_driver_wait_detach((KuTaskDriverWaitTokenV1*)instance->ticket.driver->ring);
  CHECK(!ku_task_driver_lock(instance->ticket.driver));
  memcpy(instance->ticket.driver->ring, saved_ring, sizeof(saved_ring));
  CHECK(!ku_task_driver_unlock(instance->ticket.driver));
  CHECK(alias_status == KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_assert_armed(role);
  CHECK(ku_task_driver_wait_detach(&role->wait) == KU_TASK_DRIVER_OK);
  FixtureLedger after = fixture_ledger(); CHECK(before.calls == after.calls && before.bytes == after.bytes);
  role->arm_status = KU_TASK_DRIVER_OK;
}

static void fixture_probe(FixtureRole* role, KuTaskInstance_0* instance) {
  FixtureRole* child = &fixture_roles[role->target];
  if (role->probe == PROBE_SELF) child = role;
  if (role->probe == PROBE_HEADERS) { fixture_header_probe(role, instance, child); return; }
  FixtureLedger before = fixture_ledger();
  uint32_t expected = role->probe == PROBE_SELF ? KU_TASK_DRIVER_WAIT_CYCLE
      : role->probe == PROBE_CROSS ? KU_TASK_DRIVER_INVALID_ARGUMENT
      : role->probe == PROBE_OCCUPIED ? KU_TASK_DRIVER_INVALID_STATE : KU_TASK_DRIVER_PENDING;
  uint32_t result = ku_task_driver_wait_arm(&instance->ticket, &child->handle.ticket,
      &child->handle.owner.lease, &role->wait);
  CHECK(result == expected); role->arm_status = result;
  if (role->probe == PROBE_DAMAGED_PAIR) {
    fixture_assert_armed(role); role->old_wait = role->wait;
    CHECK(!ku_task_driver_lock(instance->ticket.driver));
    KuTaskDriverSlotV1* target = &instance->ticket.driver->slots[child->handle.ticket.slot];
    CHECK(target->waiter_epoch == role->wait.epoch && target->waiter_epoch < UINT64_MAX);
    uint64_t damaged_epoch = ++target->waiter_epoch;
    CHECK(!ku_task_driver_unlock(instance->ticket.driver));
    CHECK(ku_task_driver_wait_detach(&role->wait) == KU_TASK_DRIVER_INTERNAL);
    CHECK(!memcmp(&role->wait, &role->old_wait, sizeof(role->wait)));
    CHECK(!ku_task_driver_lock(instance->ticket.driver));
    CHECK(instance->ticket.driver->fault == KU_TASK_DRIVER_INTERNAL);
    CHECK(target->waiter_epoch == damaged_epoch);
    CHECK(target->waiter_parent_slot == instance->ticket.slot);
    CHECK(instance->ticket.driver->slots[instance->ticket.slot].wait_state == KU_TASK_DRIVER_WAIT_EMPTY);
    CHECK(instance->ticket.driver->slots[instance->ticket.slot].intent == KU_TASK_DRIVER_YIELD);
    CHECK(!ku_task_driver_unlock(instance->ticket.driver));
    KuTaskDriverWaitSnapshotV1 unused = {0};
    CHECK(ku_task_driver_wait_read(&role->wait, &unused) == KU_TASK_DRIVER_STALE);
    /* The failed token is only an ID, not an owner; discard this now-invalid
     * caller copy. Leave the damaged peer and sticky driver fault untouched:
     * real child cancellation/publication must remove that peer and free it. */
    role->wait = (KuTaskDriverWaitTokenV1){0}; role->arm_status = KU_TASK_DRIVER_INTERNAL;
    FixtureLedger after = fixture_ledger(); CHECK(before.calls == after.calls && before.bytes == after.bytes);
    return;
  }
  if (role->probe != PROBE_EPOCH) { CHECK(!role->wait.driver); return; }
  fixture_assert_armed(role);
  role->old_wait = role->wait;
  CHECK(ku_task_driver_wait_arm(&instance->ticket, &child->handle.ticket,
      &child->handle.owner.lease, &role->wait) == KU_TASK_DRIVER_INVALID_STATE);
  CHECK(role->wait.epoch == role->old_wait.epoch);
  CHECK(ku_task_driver_wait_detach(&role->wait) == KU_TASK_DRIVER_OK && !role->wait.driver);
  CHECK(ku_task_driver_wait_arm(&instance->ticket, &child->handle.ticket,
      &child->handle.owner.lease, &role->wait) == KU_TASK_DRIVER_PENDING);
  CHECK(role->wait.epoch > role->old_wait.epoch);
  KuTaskDriverWaitTokenV1 old = role->old_wait;
  CHECK(ku_task_driver_wait_detach(&old) == KU_TASK_DRIVER_STALE);
  CHECK(old.epoch == role->old_wait.epoch && old.driver == role->old_wait.driver);
  fixture_assert_armed(role);
  KuTaskDriverWaitSnapshotV1 untouched = { 91u, 92u, 93u, 94u };
  CHECK(ku_task_driver_wait_read(&old, &untouched) == KU_TASK_DRIVER_STALE);
  CHECK(untouched.state == 91u && untouched.outcome == 92u && untouched.child_slot == 93u && untouched.child_generation == 94u);
  CHECK(ku_task_driver_wait_detach(&role->wait) == KU_TASK_DRIVER_OK);
  CHECK(!ku_task_driver_lock(instance->ticket.driver));
  instance->ticket.driver->slots[instance->ticket.slot].wait_epoch = UINT64_MAX;
  CHECK(!ku_task_driver_unlock(instance->ticket.driver));
  CHECK(ku_task_driver_wait_arm(&instance->ticket, &child->handle.ticket,
      &child->handle.owner.lease, &role->wait) == KU_TASK_DRIVER_LIMIT);
  CHECK(!role->wait.driver);
  FixtureLedger after = fixture_ledger(); CHECK(before.calls == after.calls && before.bytes == after.bytes);
}

static uint32_t fixture_resume_hook(void* raw) {
  KuTaskInstance_0* instance = (KuTaskInstance_0*)raw;
  FixtureRole* role = fixture_role(raw); fixture_inc(&role->resumes);
  /* This real nested snapshot also detects a callback executed under queue mutex. */
  KuTaskDriverSnapshotV1 driver_snapshot = {0};
  CHECK(ku_task_driver_snapshot(instance->ticket.driver, &driver_snapshot) == KU_TASK_DRIVER_OK);
  if (fixture_count(&role->resumes) == 1) {
    CHECK(ku_test_event_set(&role->entered));
    if (role->entry_gate) CHECK(ku_test_event_wait(&role->proceed, 2000));
  }
  if (role->mode == MODE_HOLD) {
    CHECK(ku_task_driver_set_intent(&instance->ticket, KU_TASK_DRIVER_WAIT) == KU_TASK_DRIVER_OK);
    return KU_TASK_CONTROL_PENDING;
  }
  if (role->mode == MODE_PROBE) {
    fixture_probe(role, instance); role->mode = MODE_FINISH;
  } else if (role->mode == MODE_WAIT) {
    CHECK(role->target < ROLES); FixtureRole* child = &fixture_roles[role->target];
    if (!role->wait.driver) {
      role->arm_status = ku_task_driver_wait_arm(&instance->ticket, &child->handle.ticket,
          &child->handle.owner.lease, &role->wait);
      CHECK(role->arm_status == role->expected_arm);
      CHECK(ku_test_event_set(&role->armed));
      if (role->arm_status == KU_TASK_DRIVER_PENDING) {
        if (role->after_arm_gate) CHECK(ku_test_event_wait(&role->proceed, 2000));
        return KU_TASK_CONTROL_PENDING;
      }
      CHECK(role->arm_status == KU_TASK_DRIVER_WAIT_READY || role->arm_status == KU_TASK_DRIVER_WAIT_CYCLE);
    } else {
      KuTaskDriverWaitSnapshotV1 snapshot = fixture_wait_snapshot(role);
      if (snapshot.state == 1u) {
        CHECK(ku_task_driver_set_intent(&instance->ticket, KU_TASK_DRIVER_WAIT) == KU_TASK_DRIVER_OK);
        return KU_TASK_CONTROL_PENDING;
      }
      CHECK(snapshot.state == 2u && snapshot.outcome == KU_TASK_DRIVER_WAIT_READY);
      CHECK(ku_task_driver_wait_detach(&role->wait) == KU_TASK_DRIVER_OK);
    }
    if (role->take_child) {
      role->take_status = ku_task_0_take(&child->handle, &role->result);
      CHECK(role->take_status == KU_TASK_CONTROL_OK);
      CHECK(role->result.ok && role->result.value.len == 8u && role->result.value.ptr[4] == 0);
    }
    fixture_inc(&role->normal_continuations); role->mode = MODE_FINISH;
  }
  return UINT32_MAX; /* Execute the actual generated R4/R1 frame. */
}
static void fixture_cleanup_hook(void* raw) {
  FixtureRole* role = fixture_role(raw); fixture_inc(&role->cleanups);
  if (role->cleanup_gate) {
    CHECK(ku_test_event_set(&role->cleanup_entered));
    CHECK(ku_test_event_wait(&role->cleanup_proceed, 2000));
  }
  if (role->wait.driver) CHECK(ku_task_driver_wait_detach(&role->wait) == KU_TASK_DRIVER_OK);
}
static uint32_t fixture_take_hook(void* raw, void* destination) {
  (void)destination; FixtureRole* role = fixture_role(raw); fixture_inc(&role->take_calls);
  if (role->take_gate && fixture_count(&role->take_calls) == 1) {
    KuTaskInstance_0* instance = (KuTaskInstance_0*)raw;
    /* Deliberately early notification is insufficient while payload is TAKING. */
    CHECK(ku_task_driver_wake(&instance->ticket) == KU_TASK_DRIVER_OK);
    CHECK(ku_test_event_set(&role->take_entered));
    CHECK(ku_test_event_wait(&role->take_proceed, 2000));
    if (role->take_reject) return KU_TASK_CONTROL_INVALID_ARGUMENT;
  }
  return UINT32_MAX;
}
static void fixture_after_take_store(void* raw) {
  KuTaskControlV1* control = (KuTaskControlV1*)raw;
  FixtureRole* role = fixture_role(control->context);
  if (role->store_gate && fixture_count(&role->take_calls) == 1) {
    CHECK(ku_test_event_set(&role->stored)); CHECK(ku_test_event_wait(&role->store_proceed, 2000));
  }
}

typedef struct FixtureTake {
  KuTaskDriverTicketV1 ticket;
  KuTaskControlLeaseV1 lease;
  KuResult_str output;
  uint32_t status;
} FixtureTake;
static int fixture_take_thread(void* raw) {
  FixtureTake* take = (FixtureTake*)raw;
  take->status = ku_task_driver_take_result(&take->ticket, &take->lease, &take->output);
  return 0;
}
static void fixture_take_start(FixtureTake* take, KuTestThread* thread, size_t child) {
  memset(take, 0, sizeof(*take)); take->ticket = fixture_roles[child].handle.ticket;
  /* Child is quiescent terminal: retain cannot race its released lifecycle pin. */
  CHECK(ku_task_control_lease_retain(&fixture_roles[child].handle.owner.lease, &take->lease) == KU_TASK_CONTROL_OK);
  CHECK(ku_test_thread_start(thread, fixture_take_thread, take));
  CHECK(ku_test_event_wait(&fixture_roles[child].take_entered, 2000));
}
static void fixture_take_finish(FixtureTake* take, KuTestThread* thread) {
  CHECK(ku_test_thread_join(thread, 2000) && thread->outcome == 0);
  CHECK(ku_task_control_lease_release(&take->lease) == KU_TASK_CONTROL_OK);
  ku_result_drop_str(&take->output);
}

static void fixture_ready_before_arm(void) {
  FixtureDriver runtime; fixture_driver_init(&runtime, 2);
  fixture_role_init(0, MODE_FINISH); fixture_start(&runtime, 0); fixture_idle(&runtime);
  fixture_role_init(1, MODE_WAIT); fixture_roles[1].target = 0;
  fixture_roles[1].expected_arm = KU_TASK_DRIVER_WAIT_READY; fixture_roles[1].take_child = 1;
  fixture_start(&runtime, 1); fixture_idle(&runtime);
  CHECK(fixture_roles[1].arm_status == KU_TASK_DRIVER_WAIT_READY && !fixture_roles[1].wait.driver);
  CHECK(fixture_count(&fixture_roles[1].normal_continuations) == 1);
  CHECK(ku_task_control_status(&fixture_roles[1].handle.owner.lease) == KU_TASK_CONTROL_COMPLETED);
  fixture_drop(1); fixture_drop(0); fixture_driver_finish(&runtime); fixture_zero();
}

static void fixture_parked_and_late_child(void) {
  FixtureDriver runtime; fixture_driver_init(&runtime, 3);
  fixture_role_init(0, MODE_HOLD); fixture_start(&runtime, 0); fixture_idle(&runtime);
  fixture_role_init(1, MODE_HOLD); fixture_start(&runtime, 1); fixture_idle(&runtime);
  fixture_role_init(2, MODE_WAIT); fixture_roles[2].target = 0;
  fixture_start(&runtime, 2); fixture_idle(&runtime); fixture_assert_armed(&fixture_roles[2]);
  KuTaskDriverSnapshotV1 before = fixture_snapshot(&runtime);
  CHECK(before.parked == 3u && before.worker_waiting);
  /* Repeated completed idle handshakes inspect the condition predicate, not time.
   * No task-level retry/poll is allowed while no event has happened. */
  for (size_t i = 0; i < 8; ++i) { fixture_idle(&runtime); CHECK(fixture_snapshot(&runtime).polls == before.polls); }
  KuTaskDriverWaitTokenV1 old = fixture_roles[2].wait;
  /* Publish the next continuation before detach enqueues the parked parent.
   * The driver mutex makes that handoff visible to its next quantum. */
  fixture_roles[2].target = 1;
  CHECK(ku_task_driver_wait_detach(&fixture_roles[2].wait) == KU_TASK_DRIVER_OK);
  fixture_idle(&runtime); fixture_assert_armed(&fixture_roles[2]);
  CHECK(fixture_roles[2].wait.epoch > old.epoch);
  CHECK(ku_task_driver_wait_detach(&old) == KU_TASK_DRIVER_STALE);
  size_t parent_resumes = fixture_count(&fixture_roles[2].resumes);
  fixture_roles[0].mode = MODE_FINISH;
  CHECK(ku_task_driver_wake(&fixture_roles[0].handle.ticket) == KU_TASK_DRIVER_OK);
  fixture_idle(&runtime);
  CHECK(fixture_count(&fixture_roles[2].resumes) == parent_resumes);
  fixture_assert_armed(&fixture_roles[2]);
  fixture_roles[1].mode = MODE_FINISH;
  CHECK(ku_task_driver_wake(&fixture_roles[1].handle.ticket) == KU_TASK_DRIVER_OK);
  fixture_idle(&runtime); CHECK(fixture_count(&fixture_roles[2].normal_continuations) == 1);
  fixture_drop(2); fixture_drop(1); fixture_drop(0); fixture_driver_finish(&runtime); fixture_zero();
}

static void fixture_taking_publication(int reject, int park_gap) {
  FixtureDriver runtime; fixture_driver_init(&runtime, 2);
  fixture_role_init(0, MODE_FINISH); fixture_roles[0].take_gate = 1;
  fixture_roles[0].take_reject = reject; fixture_roles[0].store_gate = 1;
  fixture_start(&runtime, 0); fixture_idle(&runtime);
  FixtureTake take; KuTestThread thread; fixture_take_start(&take, &thread, 0);
  fixture_role_init(1, MODE_WAIT); fixture_roles[1].target = 0;
  fixture_roles[1].take_child = reject; fixture_roles[1].after_arm_gate = park_gap;
  fixture_start(&runtime, 1); CHECK(ku_test_event_wait(&fixture_roles[1].armed, 2000));
  if (!park_gap) fixture_idle(&runtime);
  fixture_assert_armed(&fixture_roles[1]);
  size_t parent_resumes = fixture_count(&fixture_roles[1].resumes);
  CHECK(ku_test_event_set(&fixture_roles[0].take_proceed));
  CHECK(ku_test_event_wait(&fixture_roles[0].stored, 2000));
  /* The real R2 final store has happened but its wrapper has not notified yet.
   * An old callback-only notifier cannot make this test pass. */
  fixture_assert_armed(&fixture_roles[1]);
  CHECK(fixture_count(&fixture_roles[1].resumes) == parent_resumes);
  CHECK(ku_test_event_set(&fixture_roles[0].store_proceed));
  fixture_take_finish(&take, &thread);
  CHECK(take.status == (reject ? KU_TASK_CONTROL_INVALID_ARGUMENT : KU_TASK_CONTROL_OK));
  if (park_gap) {
    KuTaskDriverWaitSnapshotV1 snapshot = fixture_wait_snapshot(&fixture_roles[1]);
    CHECK(snapshot.state == 2u && snapshot.outcome == KU_TASK_DRIVER_WAIT_READY);
    CHECK(!ku_task_driver_lock(runtime.driver));
    CHECK(runtime.slots[fixture_roles[1].handle.ticket.slot].state == KU_TASK_DRIVER_RUNNING);
    CHECK(runtime.slots[fixture_roles[1].handle.ticket.slot].notified);
    CHECK(!ku_task_driver_unlock(runtime.driver));
    CHECK(ku_test_event_set(&fixture_roles[1].proceed));
  }
  fixture_idle(&runtime); CHECK(fixture_count(&fixture_roles[1].normal_continuations) == 1);
  fixture_drop(1); fixture_drop(0); fixture_driver_finish(&runtime); fixture_zero();
}

static void fixture_epoch_and_slot_reuse(void) {
  FixtureDriver runtime; fixture_driver_init(&runtime, 2);
  fixture_role_init(0, MODE_HOLD); fixture_start(&runtime, 0); fixture_idle(&runtime);
  fixture_role_init(1, MODE_PROBE); fixture_roles[1].target = 0; fixture_roles[1].probe = PROBE_EPOCH;
  fixture_start(&runtime, 1); fixture_idle(&runtime);
  KuTaskDriverWaitTokenV1 old = fixture_roles[1].old_wait;
  KuTaskDriverTicketV1 retired = fixture_roles[1].handle.ticket;
  fixture_drop(1); fixture_idle(&runtime); CHECK(fixture_snapshot(&runtime).resident == 1u);
  fixture_role_init(2, MODE_WAIT); fixture_roles[2].target = 0;
  fixture_start(&runtime, 2); fixture_idle(&runtime); fixture_assert_armed(&fixture_roles[2]);
  CHECK(fixture_roles[2].handle.ticket.slot == retired.slot && fixture_roles[2].handle.ticket.generation > retired.generation);
  CHECK(ku_task_driver_wait_detach(&old) == KU_TASK_DRIVER_STALE);
  fixture_assert_armed(&fixture_roles[2]);
  fixture_drop(2); fixture_drop(0); fixture_driver_finish(&runtime); fixture_zero();
}

static void fixture_cycle_and_occupied(void) {
  FixtureDriver runtime; fixture_driver_init(&runtime, 4);
  for (size_t i = 0; i < 3; ++i) { fixture_role_init(i, MODE_HOLD); fixture_start(&runtime, i); }
  fixture_idle(&runtime);
  fixture_roles[0].mode = MODE_WAIT; fixture_roles[0].target = 1;
  CHECK(ku_task_driver_wake(&fixture_roles[0].handle.ticket) == KU_TASK_DRIVER_OK);
  fixture_idle(&runtime); fixture_assert_armed(&fixture_roles[0]);
  fixture_role_init(3, MODE_PROBE); fixture_roles[3].probe = PROBE_OCCUPIED; fixture_roles[3].target = 1;
  fixture_start(&runtime, 3); fixture_idle(&runtime); fixture_assert_armed(&fixture_roles[0]);
  CHECK(fixture_roles[3].arm_status == KU_TASK_DRIVER_INVALID_STATE);
  fixture_roles[1].mode = MODE_WAIT; fixture_roles[1].target = 2;
  CHECK(ku_task_driver_wake(&fixture_roles[1].handle.ticket) == KU_TASK_DRIVER_OK);
  fixture_idle(&runtime); fixture_assert_armed(&fixture_roles[1]);
  fixture_roles[2].mode = MODE_WAIT; fixture_roles[2].target = 0;
  fixture_roles[2].expected_arm = KU_TASK_DRIVER_WAIT_CYCLE;
  CHECK(ku_task_driver_wake(&fixture_roles[2].handle.ticket) == KU_TASK_DRIVER_OK);
  fixture_idle(&runtime); CHECK(fixture_roles[2].arm_status == KU_TASK_DRIVER_WAIT_CYCLE);
  CHECK(fixture_count(&fixture_roles[0].normal_continuations) == 1);
  for (size_t i = 0; i < 4; ++i) fixture_drop(i);
  fixture_driver_finish(&runtime); fixture_zero();
}

static void fixture_self_and_cross_driver(void) {
  FixtureDriver first, second; fixture_driver_init(&first, 2); fixture_driver_init(&second, 1);
  fixture_role_init(0, MODE_HOLD); fixture_start(&second, 0); fixture_idle(&second);
  fixture_role_init(1, MODE_PROBE); fixture_roles[1].probe = PROBE_CROSS; fixture_roles[1].target = 0;
  fixture_start(&first, 1); fixture_idle(&first);
  CHECK(fixture_roles[1].arm_status == KU_TASK_DRIVER_INVALID_ARGUMENT);
  fixture_role_init(2, MODE_PROBE); fixture_roles[2].probe = PROBE_SELF; fixture_roles[2].target = 2;
  fixture_roles[2].entry_gate = 1; fixture_start(&first, 2);
  CHECK(ku_test_event_wait(&fixture_roles[2].entered, 2000));
  CHECK(ku_test_event_set(&fixture_roles[2].proceed)); fixture_idle(&first);
  CHECK(fixture_roles[2].arm_status == KU_TASK_DRIVER_WAIT_CYCLE);
  fixture_drop(2); fixture_drop(1); fixture_drop(0);
  fixture_driver_finish(&first); fixture_driver_finish(&second); fixture_zero();
}

static void fixture_reference_and_queue_limits(void) {
  FixtureDriver runtime; fixture_driver_init(&runtime, 4);
  fixture_role_init(0, MODE_HOLD); fixture_start(&runtime, 0); fixture_idle(&runtime);
  KuTaskControlV1* child = fixture_roles[0].handle.owner.lease.control;
  size_t reference_count = fixture_count(&child->references);
  CHECK(reference_count < KU_TASK_CONTROL_MAX_REFERENCES);
  size_t count = KU_TASK_CONTROL_MAX_REFERENCES - reference_count;
  KuTaskControlLeaseV1* leases = (KuTaskControlLeaseV1*)calloc(count, sizeof(*leases)); CHECK(leases);
  for (size_t i = 0; i < count; ++i)
    CHECK(ku_task_control_lease_retain(&fixture_roles[0].handle.owner.lease, &leases[i]) == KU_TASK_CONTROL_OK);
  CHECK(fixture_count(&child->references) == KU_TASK_CONTROL_MAX_REFERENCES);
  fixture_role_init(1, MODE_WAIT); fixture_roles[1].target = 0; fixture_roles[1].entry_gate = 1;
  fixture_start(&runtime, 1); CHECK(ku_test_event_wait(&fixture_roles[1].entered, 2000));
  for (size_t i = 2; i < 4; ++i) { fixture_role_init(i, MODE_FINISH); fixture_start(&runtime, i); }
  CHECK(ku_task_driver_wake(&fixture_roles[0].handle.ticket) == KU_TASK_DRIVER_OK);
  KuTaskDriverTicketV1 refused = {0};
  CHECK(ku_task_driver_reserve(runtime.driver, 1u, &refused) == KU_TASK_DRIVER_LIMIT && !refused.driver);
  KuTaskDriverSnapshotV1 full = fixture_snapshot(&runtime);
  CHECK(full.resident == runtime.capacity && full.running == 1u && full.queued == runtime.capacity - 1u);
  FixtureLedger before = fixture_ledger();
  CHECK(ku_test_event_set(&fixture_roles[1].proceed)); fixture_idle(&runtime);
  fixture_assert_armed(&fixture_roles[1]);
  CHECK(fixture_count(&child->references) == KU_TASK_CONTROL_MAX_REFERENCES);
  CHECK(fixture_ledger().calls == before.calls);
  for (size_t i = 0; i < count; ++i) CHECK(ku_task_control_lease_release(&leases[i]) == KU_TASK_CONTROL_OK);
  free(leases);
  /* Full admission does not prevent cancellation/detachment or eventual wake. */
  fixture_drop(1); fixture_roles[0].mode = MODE_FINISH;
  CHECK(ku_task_driver_wake(&fixture_roles[0].handle.ticket) == KU_TASK_DRIVER_OK);
  fixture_idle(&runtime); CHECK(!fixture_count(&fixture_roles[1].normal_continuations));
  fixture_drop(3); fixture_drop(2); fixture_drop(0); fixture_driver_finish(&runtime); fixture_zero();
}

static void fixture_cancel_keeps_incoming(uint32_t reason) {
  FixtureDriver runtime; fixture_driver_init(&runtime, 3);
  fixture_role_init(0, MODE_HOLD); fixture_start(&runtime, 0); fixture_idle(&runtime);
  fixture_role_init(1, MODE_WAIT); fixture_roles[1].target = 0; fixture_roles[1].cleanup_gate = 1;
  fixture_start(&runtime, 1); fixture_idle(&runtime);
  fixture_role_init(2, MODE_WAIT); fixture_roles[2].target = 1;
  fixture_start(&runtime, 2); fixture_idle(&runtime);
  fixture_assert_armed(&fixture_roles[1]); fixture_assert_armed(&fixture_roles[2]);
  size_t normal_resumes = fixture_count(&fixture_roles[1].resumes);
  CHECK(ku_task_driver_request_cancel(&fixture_roles[1].handle.ticket,
      &fixture_roles[1].handle.owner.lease, reason, fixture_deadline()) == KU_TASK_CONTROL_OK);
  CHECK(ku_test_event_wait(&fixture_roles[1].cleanup_entered, 2000));
  KuTaskDriverWaitSnapshotV1 aborted = fixture_wait_snapshot(&fixture_roles[1]);
  CHECK(aborted.state == 2u && aborted.outcome == KU_TASK_DRIVER_WAIT_ABORTED);
  fixture_assert_armed(&fixture_roles[2]);
  CHECK(!ku_task_driver_lock(runtime.driver));
  KuTaskDriverSlotV1* parent = &runtime.slots[fixture_roles[1].handle.ticket.slot];
  CHECK(parent->waiter_parent_generation == fixture_roles[2].handle.ticket.generation);
  CHECK(parent->waiter_epoch == fixture_roles[2].wait.epoch);
  CHECK(!runtime.slots[fixture_roles[0].handle.ticket.slot].waiter_epoch);
  CHECK(!ku_task_driver_unlock(runtime.driver));
  CHECK(ku_task_driver_request_cancel(&fixture_roles[1].handle.ticket,
      &fixture_roles[1].handle.owner.lease, reason, fixture_deadline()) == KU_TASK_CONTROL_OK);
  CHECK(ku_test_event_set(&fixture_roles[1].cleanup_proceed)); fixture_idle(&runtime);
  CHECK(ku_task_control_status(&fixture_roles[1].handle.owner.lease) == reason);
  CHECK(fixture_count(&fixture_roles[1].resumes) == normal_resumes);
  CHECK(!fixture_count(&fixture_roles[1].normal_continuations) && fixture_count(&fixture_roles[1].cleanups) == 1u);
  CHECK(fixture_count(&fixture_roles[2].normal_continuations) == 1u);
  fixture_roles[0].mode = MODE_FINISH;
  CHECK(ku_task_driver_wake(&fixture_roles[0].handle.ticket) == KU_TASK_DRIVER_OK); fixture_idle(&runtime);
  CHECK(fixture_count(&fixture_roles[1].resumes) == normal_resumes);
  fixture_drop(2); fixture_drop(1); fixture_drop(0); fixture_driver_finish(&runtime); fixture_zero();
}

static void fixture_headers_and_damaged_pair(void) {
  FixtureDriver runtime; fixture_driver_init(&runtime, 4);
  fixture_role_init(0, MODE_HOLD); fixture_start(&runtime, 0); fixture_idle(&runtime);
  fixture_role_init(1, MODE_PROBE); fixture_roles[1].target = 0; fixture_roles[1].probe = PROBE_HEADERS;
  fixture_start(&runtime, 1); fixture_idle(&runtime);
  CHECK(fixture_roles[1].arm_status == KU_TASK_DRIVER_OK);
  fixture_drop(1); fixture_drop(0); fixture_driver_finish(&runtime); fixture_zero();

  fixture_driver_init(&runtime, 2);
  fixture_role_init(0, MODE_HOLD); fixture_start(&runtime, 0); fixture_idle(&runtime);
  fixture_role_init(1, MODE_PROBE); fixture_roles[1].target = 0; fixture_roles[1].probe = PROBE_DAMAGED_PAIR;
  fixture_start(&runtime, 1);
  CHECK(ku_task_driver_wait_idle(runtime.driver, fixture_deadline()) == KU_TASK_DRIVER_INTERNAL);
  CHECK(fixture_roles[1].arm_status == KU_TASK_DRIVER_INTERNAL);
  KuTaskDriverSnapshotV1 snapshot = {0};
  CHECK(ku_task_driver_snapshot(runtime.driver, &snapshot) == KU_TASK_DRIVER_OK);
  CHECK(snapshot.fault == KU_TASK_DRIVER_INTERNAL && !snapshot.queued && !snapshot.running && snapshot.worker_waiting);
  fixture_drop(1); fixture_drop(0);
  fixture_driver_finish_expected(&runtime, KU_TASK_DRIVER_INTERNAL); fixture_zero();
}

int main(void) {
  CHECK(KU_TASK_DRIVER_ABI_VERSION == 3u);
  fixture_ready_before_arm();
  fixture_parked_and_late_child();
  fixture_taking_publication(0, 0); fixture_taking_publication(1, 0);
  fixture_taking_publication(0, 1); fixture_taking_publication(1, 1);
  fixture_epoch_and_slot_reuse(); fixture_cycle_and_occupied(); fixture_self_and_cross_driver();
  fixture_reference_and_queue_limits();
  fixture_cancel_keeps_incoming(KU_TASK_CONTROL_CANCELLED);
  fixture_cancel_keeps_incoming(KU_TASK_CONTROL_TIMED_OUT);
  fixture_headers_and_damaged_pair();
  puts("task-wait-v1-ok"); return 0;
}
"#;
