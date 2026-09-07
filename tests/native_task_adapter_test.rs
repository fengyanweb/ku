//! R4 typed factories over verified frames, not source async or public Task syntax.

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

fn slot(ty: IrType) -> TaskSlot {
    TaskSlot {
        ty: TaskSlotType::Value {
            ty,
            borrowed: false,
        },
    }
}

fn result(ty: IrType) -> IrType {
    IrType::Result(Box::new(ty))
}

fn fixture_source() -> String {
    let ast = Parser::new(Lexer::new("fn main() {}").lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let sync = ir::lower_program(&ast).unwrap();
    let mut tasks = TaskProgram { functions: vec![] };
    for primitive in [IrType::Int, IrType::Bool, IrType::Null, IrType::Str] {
        let id = tasks.functions.len();
        tasks.functions.push(TaskFunction {
            id: TaskFunctionId(id),
            name: format!("Primitive{id}"),
            slots: vec![slot(primitive.clone()), slot(result(primitive.clone()))],
            parameters: vec![SlotId(0)],
            entry: StateId(0),
            result: result(primitive),
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
                        dst: SlotId(1),
                        src: SlotId(0),
                    }],
                    terminator: TaskTerminator::Complete { value: SlotId(1) },
                },
                TaskState {
                    operations: vec![
                        TaskOp::DropIfInit { slot: SlotId(1) },
                        TaskOp::DropIfInit { slot: SlotId(0) },
                    ],
                    terminator: TaskTerminator::Terminate,
                },
            ],
        });
    }
    for primitive in [IrType::Int, IrType::Bool, IrType::Null, IrType::Str] {
        let id = tasks.functions.len();
        let output = result(primitive);
        tasks.functions.push(TaskFunction {
            id: TaskFunctionId(id),
            name: format!("Result{id}"),
            slots: vec![slot(output.clone())],
            parameters: vec![SlotId(0)],
            entry: StateId(0),
            result: output,
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
                    terminator: TaskTerminator::Complete { value: SlotId(0) },
                },
                TaskState {
                    operations: vec![TaskOp::Drop { slot: SlotId(0) }],
                    terminator: TaskTerminator::Terminate,
                },
            ],
        });
    }
    let mut sparse = vec![slot(IrType::Int); 64];
    sparse[2] = slot(IrType::Bool);
    sparse[5] = slot(IrType::Null);
    sparse[9] = slot(IrType::Str);
    sparse[63] = slot(result(IrType::Str));
    tasks.functions.push(TaskFunction {
        id: TaskFunctionId(8),
        name: "SparseParameters".into(),
        slots: sparse,
        parameters: vec![SlotId(63), SlotId(2), SlotId(9), SlotId(0), SlotId(5)],
        entry: StateId(0),
        result: result(IrType::Str),
        states: vec![
            TaskState {
                operations: vec![],
                terminator: TaskTerminator::Suspend {
                    resume: StateId(1),
                    cleanup: StateId(2),
                },
            },
            TaskState {
                operations: vec![
                    TaskOp::Read { slot: SlotId(0) },
                    TaskOp::Read { slot: SlotId(2) },
                    TaskOp::Read { slot: SlotId(5) },
                    TaskOp::Drop { slot: SlotId(9) },
                ],
                terminator: TaskTerminator::Complete { value: SlotId(63) },
            },
            TaskState {
                operations: vec![
                    TaskOp::Drop { slot: SlotId(9) },
                    TaskOp::Drop { slot: SlotId(63) },
                ],
                terminator: TaskTerminator::Terminate,
            },
        ],
    });
    tasks.functions.push(TaskFunction {
        id: TaskFunctionId(9),
        name: "CopyHeaderAlias".into(),
        slots: vec![
            slot(IrType::Int),
            slot(IrType::Int),
            slot(result(IrType::Int)),
        ],
        parameters: vec![SlotId(0), SlotId(1)],
        entry: StateId(0),
        result: result(IrType::Int),
        states: vec![TaskState {
            operations: vec![
                TaskOp::Read { slot: SlotId(1) },
                TaskOp::WrapOk {
                    dst: SlotId(2),
                    src: SlotId(0),
                },
            ],
            terminator: TaskTerminator::Complete { value: SlotId(2) },
        }],
    });
    tasks.functions.push(TaskFunction {
        id: TaskFunctionId(10),
        name: "LiveCleanupBudget".into(),
        slots: vec![
            slot(result(IrType::Str)),
            slot(IrType::Str),
            slot(IrType::Bool),
        ],
        parameters: vec![SlotId(0), SlotId(1), SlotId(2)],
        entry: StateId(0),
        result: result(IrType::Str),
        states: vec![
            TaskState {
                operations: vec![],
                terminator: TaskTerminator::Suspend {
                    resume: StateId(1),
                    cleanup: StateId(2),
                },
            },
            TaskState {
                operations: vec![TaskOp::Drop { slot: SlotId(1) }],
                terminator: TaskTerminator::Complete { value: SlotId(0) },
            },
            TaskState {
                operations: vec![TaskOp::Read { slot: SlotId(1) }],
                terminator: TaskTerminator::Jump { target: StateId(3) },
            },
            TaskState {
                operations: vec![
                    TaskOp::Read { slot: SlotId(1) },
                    TaskOp::Read { slot: SlotId(2) },
                ],
                terminator: TaskTerminator::Jump { target: StateId(4) },
            },
            TaskState {
                operations: vec![
                    TaskOp::Drop { slot: SlotId(1) },
                    TaskOp::Drop { slot: SlotId(0) },
                ],
                terminator: TaskTerminator::Terminate,
            },
        ],
    });
    c::generate_task_frame_c_source(&sync, &tasks).unwrap()
}

#[test]
fn native_task_adapter_factory_ownership_budget_and_failure_execute_in_c() {
    let generated = fixture_source();
    let adapter_anchor = "/* Internal typed adapter ABI; not source Task syntax or a stable C FFI.";
    let adapter_start = generated
        .find(adapter_anchor)
        .expect("generated adapter ABI");
    let instance_start = generated
        .find("typedef struct KuTaskInstance_0 {")
        .expect("generated typed instance");
    let adapter_end = generated.find("int main(void) {").expect("generated main");
    let adapter = &generated[adapter_start..adapter_end];
    for forbidden in [
        "run_source",
        "const SOURCE",
        "task.spawn",
        "Task.new",
        "exit(",
        "abort(",
    ] {
        assert!(
            !adapter.contains(forbidden),
            "forbidden adapter path {forbidden}"
        );
    }
    assert!(adapter.contains("KU_TASK_DRIVER_YIELD"));
    // Shared drain code now supports hosted Tasks, but these Value-only R4
    // frames still suspend by yielding and cannot register a source wait.
    for id in 0..11 {
        let resume_signature = format!("static uint32_t ku_task_{id}_resume(void* raw) {{");
        let cleanup_signature = format!("static uint32_t ku_task_{id}_cleanup(");
        let resume = adapter
            .split_once(resume_signature.as_str())
            .expect("concrete adapter resume")
            .1
            .split_once(cleanup_signature.as_str())
            .expect("concrete adapter cleanup boundary")
            .0;
        assert!(resume.contains("KU_TASK_DRIVER_YIELD"));
        assert!(!resume.contains("KU_TASK_DRIVER_WAIT"));
    }
    let factories = &generated[instance_start..adapter_end];
    assert_eq!(
        factories.matches("calloc(").count(),
        11,
        "one inline instance allocation per factory"
    );
    assert!(!factories.contains("malloc(") && !factories.contains("realloc("));
    let instrumentation = format!(
        "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER}\n{ALLOCATION_HOOK}\n{LOCKED_ALLOCATIONS}"
    );
    let mut source = generated
        .replacen(
            "typedef struct KuString {",
            &format!("{instrumentation}\ntypedef struct KuString {{"),
            1,
        )
        .replacen(
            adapter_anchor,
            &format!("{FACTORY_HOOKS}\n{adapter_anchor}"),
            1,
        )
        .replacen(
            "int main(void) {",
            &format!("{FACTORY_UNDEFS}\nstatic int ku_generated_main(void) {{"),
            1,
        );
    source.push_str(C_MAIN);
    let directory = TempDir::new("task-adapter-v1");
    let c_file = directory.path().join("adapter.c");
    fs::write(&c_file, source).expect("write generated adapter fixture");
    let Some(executable) = compile_harness(directory.path(), &c_file, "adapter") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute generated adapters"
        );
        return;
    };
    fs::remove_file(&c_file).expect("remove C source before executing artifact");
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("adapter fixture must remain bounded");
    assert!(
        output.status.success(),
        "adapter fixture failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-adapter-v1-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const LEDGER: &str = r#"
#if defined(_WIN32)
static SRWLOCK fixture_allocation_lock = SRWLOCK_INIT;
static void fixture_alloc_lock(void) { AcquireSRWLockExclusive(&fixture_allocation_lock); }
static void fixture_alloc_unlock(void) { ReleaseSRWLockExclusive(&fixture_allocation_lock); }
#else
static pthread_mutex_t fixture_allocation_lock = PTHREAD_MUTEX_INITIALIZER;
static void fixture_alloc_lock(void) { if (pthread_mutex_lock(&fixture_allocation_lock)) abort(); }
static void fixture_alloc_unlock(void) { if (pthread_mutex_unlock(&fixture_allocation_lock)) abort(); }
#endif
"#;

const LOCKED_ALLOCATIONS: &str = r#"
#undef malloc
#undef calloc
#undef realloc
#undef free
static size_t fixture_allocation_attempts;
static int fixture_fail_next_allocation;
static int fixture_allocation_fails(void) {
  ++fixture_allocation_attempts;
  if (!fixture_fail_next_allocation) return 0;
  fixture_fail_next_allocation = 0; return 1;
}
static void* fixture_malloc(size_t size) {
  fixture_alloc_lock();
  void* result = fixture_allocation_fails() ? NULL : ku_perf_malloc(size);
  fixture_alloc_unlock(); return result;
}
static void* fixture_calloc(size_t count, size_t size) {
  fixture_alloc_lock();
  void* result = fixture_allocation_fails() ? NULL : ku_perf_calloc(count, size);
  fixture_alloc_unlock(); return result;
}
static void* fixture_realloc(void* pointer, size_t size) {
  fixture_alloc_lock();
  void* result = fixture_allocation_fails() ? NULL : ku_perf_realloc(pointer, size);
  fixture_alloc_unlock(); return result;
}
static void fixture_free(void* pointer) {
  fixture_alloc_lock(); ku_perf_free(pointer); fixture_alloc_unlock();
}
typedef struct FixtureLedger { size_t allocations, bytes, attempts; int overflow; } FixtureLedger;
static FixtureLedger fixture_ledger(void) {
  fixture_alloc_lock();
  FixtureLedger ledger = { ku_perf_live_allocations, ku_perf_live_bytes, fixture_allocation_attempts, ku_perf_overflow };
  fixture_alloc_unlock(); return ledger;
}
static void fixture_fail_allocation(void) {
  fixture_alloc_lock(); fixture_fail_next_allocation = 1; fixture_alloc_unlock();
}
#define malloc fixture_malloc
#define calloc fixture_calloc
#define realloc fixture_realloc
#define free fixture_free
"#;

const FACTORY_HOOKS: &str = r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr, "adapter check line %d: %s\n", __LINE__, #c); abort(); } } while (0)
/* Instrument calls, not adapter behavior. Every success delegates to the real
 * generated frame/control/driver functions; no test implements an adapter. */
enum { FIXTURE_NO_FAULT, FIXTURE_CONTROL_FAIL, FIXTURE_FRAME_FAIL, FIXTURE_COMMIT_FAIL, FIXTURE_COMMIT_GATE };
static unsigned fixture_fault, fixture_fault_calls, fixture_private_polls, fixture_private_drops;
static KuString* fixture_moved_input;
static KuResult_str* fixture_moved_result;
static KuTestEvent fixture_entered, fixture_proceed, fixture_clock_entered, fixture_clock_proceed;
static unsigned fixture_resume_gate, fixture_resume_calls, fixture_other_resume_calls;
static unsigned fixture_clock_gate, fixture_clock_calls, fixture_cleanup_expired;
static uint64_t fixture_cleanup_initial_deadline;
static uint32_t fixture_control_init(KuTaskControlV1* control, size_t bytes, uint32_t abi,
    const KuTaskControlOpsV1* operations, void* context, KuTaskControlOwnerV1* output) {
  if (fixture_fault == FIXTURE_CONTROL_FAIL) { ++fixture_fault_calls; return KU_TASK_CONTROL_INVALID_ARGUMENT; }
  return ku_task_control_init(control, bytes, abi, operations, context, output);
}
static uint32_t fixture_frame_init(void* storage, size_t bytes, uint32_t abi, KuString* input) {
  if (fixture_fault == FIXTURE_FRAME_FAIL) { ++fixture_fault_calls; return KU_TASK_FRAME_INVALID_ARGUMENT; }
  return ku_task_frame_3_init(storage, bytes, abi, input);
}
static uint32_t fixture_commit(const KuTaskDriverTicketV1* ticket, KuTaskControlOwnerV1* owner,
                               uint32_t mode, uint64_t deadline) {
  if (fixture_fault == FIXTURE_COMMIT_FAIL || fixture_fault == FIXTURE_COMMIT_GATE) {
    ++fixture_fault_calls;
    CHECK(mode == KU_TASK_DRIVER_START && fixture_moved_input && !fixture_moved_input->ptr);
    if (fixture_moved_result)
      CHECK(!fixture_moved_result->ok && !fixture_moved_result->value.ptr
          && !fixture_moved_result->error.domain.ptr && !fixture_moved_result->error.code.ptr
          && !fixture_moved_result->error.message.ptr);
    CHECK(ku_task_control_atomic_load(&owner->lease.control->references) == 2);
    CHECK(ku_task_control_atomic_load(&owner->lease.control->lifecycle_pin) == 1);
    if (fixture_fault == FIXTURE_COMMIT_FAIL) return KU_TASK_DRIVER_INTERNAL;
    CHECK(ku_test_event_set(&fixture_entered));
    CHECK(ku_test_event_wait(&fixture_proceed, 2000));
  }
  return ku_task_driver_commit(ticket, owner, mode, deadline);
}
static uint32_t fixture_private_poll(const KuTaskControlLeaseV1* lease) {
  CHECK(fixture_fault == FIXTURE_FRAME_FAIL || fixture_fault == FIXTURE_COMMIT_FAIL);
  CHECK(ku_task_control_atomic_load(&lease->control->lifecycle_pin) == 1);
  uint32_t result = ku_task_control_poll(lease);
  CHECK(result == KU_TASK_CONTROL_CANCELLED);
  CHECK(ku_task_control_atomic_load(&lease->control->lifecycle_pin) == 0);
  CHECK(ku_task_control_atomic_load(&lease->control->references) == 1);
  ++fixture_private_polls; return result;
}
static uint32_t fixture_private_drop(KuTaskControlOwnerV1* owner, uint64_t deadline) {
  CHECK(fixture_fault == FIXTURE_FRAME_FAIL || fixture_fault == FIXTURE_COMMIT_FAIL);
  CHECK(ku_task_control_atomic_load(&owner->lease.control->references) == 1);
  uint32_t result = ku_task_control_owner_drop(owner, deadline);
  CHECK(result == KU_TASK_CONTROL_OK && !owner->lease.control);
  ++fixture_private_drops; return result;
}
static uint32_t fixture_resume_zero(void* storage, size_t bytes, uint32_t abi, const KuTaskFrameClockV1* clock) {
  if (fixture_resume_gate == 1) {
    ++fixture_resume_calls;
    if (fixture_resume_calls == 1) {
      CHECK(ku_test_event_set(&fixture_entered));
      CHECK(ku_test_event_wait(&fixture_proceed, 2000));
    } else CHECK(fixture_other_resume_calls > 0);
  }
  return ku_task_frame_0_resume(storage, bytes, abi, clock);
}
static uint32_t fixture_resume_one(void* storage, size_t bytes, uint32_t abi, const KuTaskFrameClockV1* clock) {
  if (fixture_resume_gate == 1) ++fixture_other_resume_calls;
  return ku_task_frame_1_resume(storage, bytes, abi, clock);
}
static uint32_t fixture_resume_three(void* storage, size_t bytes, uint32_t abi, const KuTaskFrameClockV1* clock) {
  CHECK(fixture_fault == FIXTURE_NO_FAULT);
  uint32_t result = ku_task_frame_3_resume(storage, bytes, abi, clock);
  if (fixture_resume_gate == 3 && result == KU_TASK_FRAME_READY) {
    ++fixture_resume_calls;
    CHECK(ku_test_event_set(&fixture_entered));
    CHECK(ku_test_event_wait(&fixture_proceed, 2000));
  }
  return result;
}
static uint32_t fixture_resume_eight(void* storage, size_t bytes, uint32_t abi, const KuTaskFrameClockV1* clock) {
  CHECK(fixture_fault == FIXTURE_NO_FAULT);
  return ku_task_frame_8_resume(storage, bytes, abi, clock);
}
static uint32_t fixture_resume_ten(void* storage, size_t bytes, uint32_t abi, const KuTaskFrameClockV1* clock) {
  uint32_t result = ku_task_frame_10_resume(storage, bytes, abi, clock);
  if (fixture_resume_gate == 10 && result == KU_TASK_FRAME_PENDING) {
    ++fixture_resume_calls;
    CHECK(ku_test_event_set(&fixture_entered));
    CHECK(ku_test_event_wait(&fixture_proceed, 2000));
  }
  return result;
}
static uint64_t fixture_adapter_now(void) {
  if (fixture_clock_gate == 2) return UINT64_MAX;
  if (fixture_clock_gate) {
    ++fixture_clock_calls;
    CHECK(fixture_clock_calls <= 8); /* This verified cleanup CFG is finite. */
    if (fixture_clock_calls == 1) {
      CHECK(ku_test_event_set(&fixture_clock_entered));
      CHECK(ku_test_event_wait(&fixture_clock_proceed, 2000));
    }
    return 1000;
  }
  return ku_task_driver_now_ms();
}
static uint32_t fixture_terminate_ten(void* storage, size_t bytes, uint32_t abi, uint32_t reason,
                                    uint64_t deadline, const KuTaskFrameClockV1* clock) {
  fixture_cleanup_initial_deadline = deadline;
  uint32_t result = ku_task_frame_10_terminate(storage, bytes, abi, reason, deadline, clock);
  fixture_cleanup_expired = ((KuTaskFrame_10*)storage)->header.cleanup_timed_out;
  return result;
}
#define ku_task_control_init fixture_control_init
#define ku_task_frame_3_init fixture_frame_init
#define ku_task_driver_commit fixture_commit
#define ku_task_control_poll fixture_private_poll
#define ku_task_control_owner_drop fixture_private_drop
#define ku_task_frame_0_resume fixture_resume_zero
#define ku_task_frame_1_resume fixture_resume_one
#define ku_task_frame_3_resume fixture_resume_three
#define ku_task_frame_8_resume fixture_resume_eight
#define ku_task_frame_10_resume fixture_resume_ten
#define ku_task_frame_10_terminate fixture_terminate_ten
#define ku_task_driver_now_ms fixture_adapter_now
"#;

const FACTORY_UNDEFS: &str = r#"
#undef ku_task_control_init
#undef ku_task_frame_3_init
#undef ku_task_driver_commit
#undef ku_task_control_poll
#undef ku_task_control_owner_drop
#undef ku_task_frame_0_resume
#undef ku_task_frame_1_resume
#undef ku_task_frame_3_resume
#undef ku_task_frame_8_resume
#undef ku_task_frame_10_resume
#undef ku_task_frame_10_terminate
#undef ku_task_driver_now_ms
"#;

const C_MAIN: &str = r#"
typedef struct FixtureRuntime { KuTaskDriverV1 driver; KuTaskDriverSlotV1 slots[4]; size_t ring[4]; } FixtureRuntime;
static uint64_t fixture_deadline(void) { return ku_task_driver_now_ms() + 2000; }
static KuTaskDriverSnapshotV1 fixture_snapshot(FixtureRuntime* runtime) {
  KuTaskDriverSnapshotV1 snapshot;
  CHECK(ku_task_driver_snapshot(&runtime->driver, &snapshot) == KU_TASK_DRIVER_OK);
  CHECK(!snapshot.fault && snapshot.resident <= 4 && snapshot.queued <= 4 && snapshot.running <= 1);
  return snapshot;
}
static void fixture_idle(FixtureRuntime* runtime) {
  CHECK(ku_task_driver_wait_idle(&runtime->driver, fixture_deadline()) == KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot = fixture_snapshot(runtime);
  CHECK(!snapshot.running && !snapshot.queued && (snapshot.worker_waiting || snapshot.worker_exited));
}
static void fixture_runtime_init(FixtureRuntime* runtime, size_t budget) {
  memset(runtime, 0, sizeof(*runtime));
  size_t fixed = sizeof(runtime->driver) + sizeof(runtime->slots) + sizeof(runtime->ring);
  CHECK(budget <= SIZE_MAX - fixed);
  CHECK(ku_task_driver_init(&runtime->driver, sizeof(runtime->driver), KU_TASK_DRIVER_ABI_VERSION,
      runtime->slots, 4, runtime->ring, 4, fixed + budget) == KU_TASK_DRIVER_OK);
  fixture_idle(runtime);
}
static void fixture_empty(FixtureRuntime* runtime) {
  fixture_idle(runtime);
  KuTaskDriverSnapshotV1 snapshot = fixture_snapshot(runtime);
  CHECK(!snapshot.resident && !snapshot.building && !snapshot.reserved_bytes);
}
static void fixture_runtime_finish(FixtureRuntime* runtime) {
  fixture_empty(runtime);
  CHECK(ku_task_driver_shutdown(&runtime->driver, fixture_deadline()) == KU_TASK_DRIVER_OK);
  uint64_t deadline = fixture_deadline();
  for (unsigned attempt = 0; attempt < 4096; ++attempt) {
    uint32_t result = ku_task_driver_destroy(&runtime->driver);
    if (result == KU_TASK_DRIVER_OK) return;
    CHECK(result == KU_TASK_DRIVER_PENDING && ku_task_driver_now_ms() < deadline);
    ku_test_thread_yield();
  }
  CHECK(false);
}
static KuTaskDriverSnapshotV1 fixture_fault_idle(FixtureRuntime* runtime, bool exited) {
  /* Public idle correctly reports the sticky fault immediately. Wait on the
   * actual protected worker predicate with the healthy test-caller clock. */
  CHECK(ku_task_driver_lock(&runtime->driver) == 0);
  uint64_t deadline = fixture_deadline();
  while (runtime->driver.queued || runtime->driver.running
      || (exited ? !runtime->driver.worker_exited : !runtime->driver.worker_waiting)) {
    CHECK(ku_task_driver_wait(&runtime->driver, deadline) >= 0);
    CHECK(ku_task_driver_now_ms() < deadline);
  }
  CHECK(ku_task_driver_unlock(&runtime->driver) == 0);
  KuTaskDriverSnapshotV1 snapshot;
  CHECK(ku_task_driver_snapshot(&runtime->driver, &snapshot) == KU_TASK_DRIVER_OK);
  CHECK(snapshot.fault == KU_TASK_DRIVER_INTERNAL && snapshot.clock_fault && snapshot.closing);
  CHECK(!snapshot.queued && !snapshot.running);
  return snapshot;
}
static void fixture_fault_finish(FixtureRuntime* runtime) {
  KuTaskDriverSnapshotV1 snapshot = fixture_fault_idle(runtime, true);
  CHECK(!snapshot.resident && !snapshot.reserved_bytes && !snapshot.building && snapshot.worker_exited);
  CHECK(ku_task_driver_shutdown(&runtime->driver, fixture_deadline()) == KU_TASK_DRIVER_INTERNAL);
  uint64_t deadline = fixture_deadline();
  for (unsigned attempt = 0; attempt < 4096; ++attempt) {
    uint32_t result = ku_task_driver_destroy(&runtime->driver);
    if (result == KU_TASK_DRIVER_OK) return;
    CHECK(result == KU_TASK_DRIVER_PENDING && ku_task_driver_now_ms() < deadline);
    ku_test_thread_yield();
  }
  CHECK(false);
}
static void fixture_ledger_zero(void) {
  FixtureLedger ledger = fixture_ledger();
  CHECK(!ledger.allocations && !ledger.bytes && !ledger.overflow);
}
static KuString fixture_owned(const char* text, size_t capacity) {
  size_t length = strlen(text); CHECK(capacity >= length && capacity > 0);
  uint8_t* data = (uint8_t*)malloc(capacity); CHECK(data);
  memcpy(data, text, length);
  return (KuString){ data, length, capacity, KU_STRING_OWNED };
}
static KuError fixture_error(void) {
  return (KuError){ fixture_owned("domain", 32), fixture_owned("code", 16), fixture_owned("message", 64) };
}
static int fixture_same_string(KuString left, KuString right) {
  return left.ptr == right.ptr && left.len == right.len && left.capacity == right.capacity && left.storage == right.storage;
}
static int fixture_same_result(KuResult_str left, KuResult_str right) {
  return left.ok == right.ok && fixture_same_string(left.value, right.value)
      && fixture_same_string(left.error.domain, right.error.domain)
      && fixture_same_string(left.error.code, right.error.code)
      && fixture_same_string(left.error.message, right.error.message);
}
static void fixture_events_init(void) {
  CHECK(ku_test_event_init(&fixture_entered)); CHECK(ku_test_event_init(&fixture_proceed));
  CHECK(ku_test_event_init(&fixture_clock_entered)); CHECK(ku_test_event_init(&fixture_clock_proceed));
}
static void fixture_events_finish(void) {
  CHECK(ku_test_event_destroy(&fixture_entered)); CHECK(ku_test_event_destroy(&fixture_proceed));
  CHECK(ku_test_event_destroy(&fixture_clock_entered)); CHECK(ku_test_event_destroy(&fixture_clock_proceed));
}
#define PRIMITIVE_CASE(id, type, field, kind, expected) do { \
  FixtureRuntime runtime; fixture_runtime_init(&runtime, sizeof(KuTaskInstance_##id)); \
  type input = (expected); KuTaskHandle_##id handle = {0}; KuTaskAdapterOutcomeV1 output = {0}; \
  FixtureLedger before = fixture_ledger(); \
  CHECK(ku_task_##id##_try_start(&runtime.driver, &input, &handle) == KU_TASK_DRIVER_OK); \
  CHECK(input == (expected)); fixture_idle(&runtime); \
  CHECK(fixture_ledger().attempts == before.attempts + 1); \
  CHECK(fixture_ledger().allocations == 1 && fixture_ledger().bytes == sizeof(KuTaskInstance_##id)); \
  CHECK(fixture_snapshot(&runtime).reserved_bytes == sizeof(KuTaskInstance_##id)); \
  CHECK(ku_task_##id##_take(&handle, &output) == KU_TASK_CONTROL_OK); CHECK(output.value.field.ok && output.value.field.value == (expected)); \
  CHECK(output.result_kind == (kind) && output.exit_class == KU_TASK_EXIT_USER_RESULT && !output.has_cleanup_deadline && !output.cleanup_deadline); \
  KuTaskAdapterOutcomeV1 again = {0}; CHECK(ku_task_##id##_take(&handle, &again) == KU_TASK_CONTROL_RESULT_TAKEN); \
  CHECK(ku_task_##id##_drop(&handle, fixture_deadline()) == KU_TASK_DRIVER_OK); \
  CHECK(!handle.owner.lease.control && !handle.ticket.driver); \
  ku_task_outcome_drop(&output); fixture_runtime_finish(&runtime); fixture_ledger_zero(); \
} while (0)
static void fixture_primitives(void) {
  PRIMITIVE_CASE(0, int64_t, integer, 1u, INT64_C(-37));
  PRIMITIVE_CASE(1, bool, boolean, 2u, true);
  PRIMITIVE_CASE(2, uint8_t, null_value, 3u, 0);
  {
    FixtureRuntime runtime; fixture_runtime_init(&runtime, sizeof(KuTaskInstance_0));
    int64_t zero = 0;
    FixtureLedger before = fixture_ledger();
    /* Reading "empty output" before the header-overlap check would follow the
     * zero owner field past this real 8-byte input into a nonexistent ticket. */
    CHECK(ku_task_0_try_start(&runtime.driver, &zero, (KuTaskHandle_0*)&zero) == KU_TASK_DRIVER_INVALID_ARGUMENT);
    CHECK(zero == 0 && fixture_ledger().attempts == before.attempts);
    fixture_runtime_finish(&runtime); fixture_ledger_zero();
  }
  FixtureRuntime runtime; fixture_runtime_init(&runtime, sizeof(KuTaskInstance_9));
  int64_t input = 41; KuTaskHandle_9 handle = {0}; KuTaskAdapterOutcomeV1 output = {0};
  CHECK(ku_task_9_try_start(&runtime.driver, &input, &input, &handle) == KU_TASK_DRIVER_OK);
  CHECK(input == 41); fixture_idle(&runtime);
  CHECK(ku_task_9_take(&handle, &output) == KU_TASK_CONTROL_OK && output.value.integer.ok && output.value.integer.value == 41);
  CHECK(ku_task_9_drop(&handle, fixture_deadline()) == KU_TASK_DRIVER_OK);
  fixture_runtime_finish(&runtime); ku_task_outcome_drop(&output); fixture_ledger_zero();
}
static void fixture_strings(void) {
  for (unsigned mode = 0; mode < 3; ++mode) {
    KuString input = mode == 0 ? ku_string_static((const uint8_t*)"A\0\xe4\xb8\xad", 5)
        : mode == 1 ? fixture_owned("payload", 64)
        : ku_string_concat((KuString){0}, (KuString){0});
    if (mode == 2) CHECK(input.ptr && !input.len && !input.capacity && input.storage == KU_STRING_OWNED);
    KuString original = input;
    size_t capacity = mode == 0 ? 0 : mode == 1 ? 64 : 1;
    FixtureRuntime runtime; fixture_runtime_init(&runtime, sizeof(KuTaskInstance_3) + capacity);
    KuTaskHandle_3 handle = {0}; KuTaskAdapterOutcomeV1 output = {0};
    FixtureLedger before = fixture_ledger();
    if (mode == 2) {
      /* The real allocation is one byte, not a readable Handle. Deep alias
       * rejection must precede every typed empty-output field access. */
      CHECK(ku_task_3_try_start(&runtime.driver, &input, (KuTaskHandle_3*)input.ptr) == KU_TASK_DRIVER_INVALID_ARGUMENT);
      CHECK(fixture_same_string(input, original) && fixture_ledger().attempts == before.attempts);
    }
    CHECK(ku_task_3_try_start(&runtime.driver, &input, &handle) == KU_TASK_DRIVER_OK);
    CHECK(!input.ptr && !input.len && !input.capacity); fixture_idle(&runtime);
    CHECK(fixture_ledger().attempts == before.attempts + 1);
    CHECK(fixture_snapshot(&runtime).reserved_bytes == sizeof(KuTaskInstance_3) + capacity);
    CHECK(ku_task_3_take(&handle, &output) == KU_TASK_CONTROL_OK);
    CHECK(output.result_kind == 4u && output.exit_class == KU_TASK_EXIT_USER_RESULT && !output.has_cleanup_deadline && !output.cleanup_deadline);
    CHECK(output.value.string.ok && fixture_same_string(output.value.string.value, original));
    CHECK(ku_task_3_drop(&handle, fixture_deadline()) == KU_TASK_DRIVER_OK);
    fixture_runtime_finish(&runtime);
    CHECK(fixture_ledger().allocations == (capacity ? 1u : 0u));
    CHECK(fixture_ledger().bytes == capacity);
    ku_task_outcome_drop(&output); fixture_ledger_zero();
  }
}
#define RESULT_SCALAR_CASE(id, suffix, field, kind, expected) do { \
  for (unsigned error = 0; error < 2; ++error) { \
    KuResult_##suffix input = {0}; input.ok = !error; input.value = (expected); \
    if (error) input.error = fixture_error(); \
    else input.error.message = (KuString){ (uint8_t*)(uintptr_t)1, SIZE_MAX, SIZE_MAX, 255 }; \
    FixtureRuntime runtime; size_t charge = sizeof(KuTaskInstance_##id) + (error ? 112 : 0); \
    fixture_runtime_init(&runtime, charge); KuTaskHandle_##id handle = {0}; KuTaskAdapterOutcomeV1 output = {0}; \
    FixtureLedger before = fixture_ledger(); \
    CHECK(ku_task_##id##_try_start(&runtime.driver, &input, &handle) == KU_TASK_DRIVER_OK); \
    CHECK(!input.ok && !input.error.message.ptr); fixture_idle(&runtime); \
    CHECK(fixture_ledger().attempts == before.attempts + 1 && fixture_snapshot(&runtime).reserved_bytes == charge); \
    CHECK(ku_task_control_status(&handle.owner.lease) == (error ? KU_TASK_CONTROL_FAILED : KU_TASK_CONTROL_COMPLETED)); \
    CHECK(ku_task_##id##_take(&handle, &output) == KU_TASK_CONTROL_OK && output.value.field.ok == !error); \
    CHECK(output.result_kind == (kind) && output.exit_class == KU_TASK_EXIT_USER_RESULT && !output.has_cleanup_deadline && !output.cleanup_deadline); \
    if (!error) CHECK(output.value.field.value == (expected)); else CHECK(output.value.field.error.domain.len == 6 && output.value.field.error.code.len == 4 && output.value.field.error.message.len == 7); \
    CHECK(ku_task_##id##_drop(&handle, fixture_deadline()) == KU_TASK_DRIVER_OK); \
    fixture_runtime_finish(&runtime); ku_task_outcome_drop(&output); fixture_ledger_zero(); \
  } \
} while (0)
static void fixture_results(void) {
  RESULT_SCALAR_CASE(4, int, integer, 1u, 73); RESULT_SCALAR_CASE(5, bool, boolean, 2u, true); RESULT_SCALAR_CASE(6, null, null_value, 3u, 0);
  for (unsigned error = 0; error < 2; ++error) {
    KuResult_str input = {0}; input.ok = !error;
    if (error) {
      input.error = fixture_error();
      input.value = (KuString){ (uint8_t*)(uintptr_t)1, SIZE_MAX, SIZE_MAX, 255 };
    } else {
      input.value = fixture_owned("result", 64);
      input.error.message = (KuString){ (uint8_t*)(uintptr_t)1, SIZE_MAX, SIZE_MAX, 255 };
    }
    size_t charge = sizeof(KuTaskInstance_7) + (error ? 112 : 64);
    FixtureRuntime runtime; fixture_runtime_init(&runtime, charge);
    KuTaskHandle_7 handle = {0}; KuTaskAdapterOutcomeV1 output = {0};
    CHECK(ku_task_7_try_start(&runtime.driver, &input, &handle) == KU_TASK_DRIVER_OK);
    CHECK(!input.value.ptr && !input.error.message.ptr); fixture_idle(&runtime);
    CHECK(fixture_snapshot(&runtime).reserved_bytes == charge);
    CHECK(ku_task_7_take(&handle, &output) == KU_TASK_CONTROL_OK && output.value.string.ok == !error);
    CHECK(output.result_kind == 4u && output.exit_class == KU_TASK_EXIT_USER_RESULT && !output.has_cleanup_deadline && !output.cleanup_deadline);
    CHECK(ku_task_7_drop(&handle, fixture_deadline()) == KU_TASK_DRIVER_OK);
    fixture_runtime_finish(&runtime); ku_task_outcome_drop(&output); fixture_ledger_zero();
  }
}
static void fixture_budget_and_oom(void) {
  KuString input = fixture_owned("capacity exceeds length", 128), original = input;
  FixtureRuntime runtime;
  fixture_runtime_init(&runtime, sizeof(KuTaskInstance_3) + 127);
  KuTaskHandle_3 handle = {0};
  FixtureLedger before = fixture_ledger();
  CHECK(ku_task_3_try_start(&runtime.driver, &input, &handle) == KU_TASK_DRIVER_LIMIT);
  CHECK(fixture_same_string(input, original) && !handle.owner.lease.control && !handle.ticket.driver);
  CHECK(fixture_ledger().attempts == before.attempts);
  fixture_runtime_finish(&runtime);
  fixture_runtime_init(&runtime, sizeof(KuTaskInstance_3) + 128);
  before = fixture_ledger(); fixture_fail_allocation();
  CHECK(ku_task_3_try_start(&runtime.driver, &input, &handle) == KU_TASK_ADAPTER_OUT_OF_MEMORY);
  CHECK(fixture_same_string(input, original) && !handle.owner.lease.control && !handle.ticket.driver);
  CHECK(fixture_ledger().attempts == before.attempts + 1);
  CHECK(fixture_ledger().allocations == before.allocations && fixture_ledger().bytes == before.bytes);
  fixture_empty(&runtime);
  /* Arithmetic rejection inspects only the raw header; the fake range is never
   * dereferenced or freed, and no allocation/reservation is made. */
  size_t overflowing_capacity = SIZE_MAX - sizeof(KuTaskInstance_3) + 1;
  KuString huge = { (uint8_t*)(uintptr_t)1, 0, overflowing_capacity, KU_STRING_OWNED };
  before = fixture_ledger();
  CHECK(ku_task_3_try_start(&runtime.driver, &huge, &handle) == KU_TASK_DRIVER_LIMIT);
  CHECK(huge.ptr == (uint8_t*)(uintptr_t)1 && huge.capacity == overflowing_capacity);
  CHECK(fixture_ledger().attempts == before.attempts && !handle.owner.lease.control);
  CHECK(ku_task_3_try_start(&runtime.driver, &input, &handle) == KU_TASK_DRIVER_OK);
  CHECK(!input.ptr); fixture_idle(&runtime);
  CHECK(ku_task_3_drop(&handle, fixture_deadline()) == KU_TASK_DRIVER_OK);
  fixture_runtime_finish(&runtime); fixture_ledger_zero();
}
static void fixture_sparse_and_preflight(void) {
  FixtureRuntime runtime; fixture_runtime_init(&runtime, 4 * sizeof(KuTaskInstance_8) + 512);
  KuResult_str input = {0}; input.ok = true; input.value = fixture_owned("return", 64);
  KuString cleanup = fixture_owned("cleanup", 32), original_cleanup = cleanup;
  KuString original_value = input.value;
  bool flag = true; int64_t number = 29; uint8_t nothing = 0;
  KuTaskHandle_8 handle = {0};
  FixtureLedger before = fixture_ledger();
  CHECK(ku_task_8_try_start(&runtime.driver, &input, &flag, &input.value, &number, &nothing, &handle) != KU_TASK_DRIVER_OK);
  CHECK(ku_task_8_try_start(&runtime.driver, &input, &flag, &cleanup, &number, &nothing, (KuTaskHandle_8*)&input) != KU_TASK_DRIVER_OK);
  CHECK(ku_task_8_try_start(&runtime.driver, &input, &flag, &cleanup, &number, &nothing, (KuTaskHandle_8*)&cleanup) != KU_TASK_DRIVER_OK);
  CHECK(ku_task_8_try_start(&runtime.driver, &input, &flag, &cleanup, &number, &nothing, (KuTaskHandle_8*)&number) != KU_TASK_DRIVER_OK);
  CHECK(ku_task_8_try_start(&runtime.driver, &input, &flag, &cleanup, &number, &nothing, NULL) != KU_TASK_DRIVER_OK);
  CHECK(ku_task_8_try_start(&runtime.driver, NULL, &flag, &cleanup, &number, &nothing, &handle) != KU_TASK_DRIVER_OK);
  CHECK(ku_task_8_try_start(&runtime.driver, &input, NULL, &cleanup, &number, &nothing, &handle) != KU_TASK_DRIVER_OK);
  CHECK(ku_task_8_try_start(&runtime.driver, &input, &flag, NULL, &number, &nothing, &handle) != KU_TASK_DRIVER_OK);
  CHECK(ku_task_8_try_start(&runtime.driver, &input, &flag, &cleanup, NULL, &nothing, &handle) != KU_TASK_DRIVER_OK);
  CHECK(ku_task_8_try_start(&runtime.driver, &input, &flag, &cleanup, &number, NULL, &handle) != KU_TASK_DRIVER_OK);
  union { long double alignment; uint8_t bytes[512]; } storage = {0};
  CHECK(ku_task_8_try_start(&runtime.driver, &input, &flag, &cleanup, &number, &nothing, (KuTaskHandle_8*)(storage.bytes + 1)) != KU_TASK_DRIVER_OK);
  CHECK(ku_task_8_try_start(&runtime.driver, (KuResult_str*)(storage.bytes + 1), &flag, &cleanup, &number, &nothing, &handle) != KU_TASK_DRIVER_OK);
  CHECK(ku_task_8_try_start(&runtime.driver, &input, &flag, &cleanup, &number, &nothing, (KuTaskHandle_8*)&runtime.driver) != KU_TASK_DRIVER_OK);
  CHECK(ku_task_8_try_start(&runtime.driver, &input, &flag, &cleanup, &number, &nothing, (KuTaskHandle_8*)runtime.slots) != KU_TASK_DRIVER_OK);
  CHECK(ku_task_8_try_start(&runtime.driver, &input, &flag, &cleanup, &number, &nothing, (KuTaskHandle_8*)runtime.ring) != KU_TASK_DRIVER_OK);
  CHECK(input.ok && fixture_same_string(input.value, original_value) && fixture_same_string(cleanup, original_cleanup));
  CHECK(flag && number == 29 && !nothing && !handle.owner.lease.control);
  CHECK(fixture_ledger().attempts == before.attempts);
  fixture_empty(&runtime);
  CHECK(ku_task_8_try_start(&runtime.driver, &input, &flag, &cleanup, &number, &nothing, &handle) == KU_TASK_DRIVER_OK);
  CHECK(!input.value.ptr && !cleanup.ptr && flag && number == 29 && !nothing);
  fixture_idle(&runtime);
  CHECK(fixture_snapshot(&runtime).reserved_bytes == sizeof(KuTaskInstance_8) + 96);
  KuTaskAdapterOutcomeV1 output = {0};
  CHECK(ku_task_8_take(&handle, &output) == KU_TASK_CONTROL_OK && output.value.string.ok && fixture_same_string(output.value.string.value, original_value));
  CHECK(ku_task_8_drop(&handle, fixture_deadline()) == KU_TASK_DRIVER_OK);
  fixture_runtime_finish(&runtime); ku_task_outcome_drop(&output); fixture_ledger_zero();
}
static void fixture_header_shape_and_empty_output(void) {
  size_t owned_capacity = sizeof(KuTaskAdapterOutcomeV1) + 16;
  FixtureRuntime runtime; fixture_runtime_init(&runtime, 2 * sizeof(KuTaskInstance_3) + owned_capacity + 64);
  KuTaskHandle_3 handle = {0};
  for (unsigned variant = 0; variant < 4; ++variant) {
    KuString bad = {0};
    if (variant == 0) bad.storage = 255;
    if (variant == 1) { bad.storage = KU_STRING_OWNED; bad.len = 1; }
    if (variant == 2) { bad.storage = KU_STRING_STATIC; bad.capacity = 1; }
    if (variant == 3) bad = (KuString){ (uint8_t*)(uintptr_t)1, 2, 1, KU_STRING_OWNED };
    FixtureLedger before = fixture_ledger();
    CHECK(ku_task_3_try_start(&runtime.driver, &bad, &handle) != KU_TASK_DRIVER_OK);
    CHECK(!handle.owner.lease.control && fixture_ledger().attempts == before.attempts);
  }
  KuString input = fixture_owned("owned", owned_capacity), original = input;
  CHECK(ku_task_3_try_start(&runtime.driver, &input, &handle) == KU_TASK_DRIVER_OK);
  fixture_idle(&runtime);
  KuString second = fixture_owned("unconsumed", 32), second_original = second;
  FixtureLedger before = fixture_ledger();
  KuTaskControlV1* control = handle.owner.lease.control;
  KuTaskDriverTicketV1 ticket = handle.ticket;
  CHECK(ku_task_3_try_start(&runtime.driver, &second, &handle) != KU_TASK_DRIVER_OK);
  CHECK(fixture_same_string(second, second_original) && handle.owner.lease.control == control);
  CHECK(handle.ticket.driver == ticket.driver && handle.ticket.generation == ticket.generation && handle.ticket.slot == ticket.slot);
  CHECK(fixture_ledger().attempts == before.attempts);
  KuTaskInstance_3* instance = (KuTaskInstance_3*)control->context;
  KuTaskAdapterOutcomeV1 wrong_output = {0};
  CHECK(ku_task_0_take((KuTaskHandle_0*)&handle, &wrong_output) == KU_TASK_CONTROL_INVALID_ARGUMENT);
  CHECK(ku_task_0_drop((KuTaskHandle_0*)&handle, fixture_deadline()) == KU_TASK_CONTROL_INVALID_ARGUMENT);
  CHECK(handle.owner.lease.control == control && !wrong_output.result_kind && !wrong_output.value.integer.ok);
  KuTaskAdapterOutcomeV1 output = {0}; output.value.string.ok = true;
  CHECK(ku_task_3_take(&handle, &output) == KU_TASK_CONTROL_INVALID_ARGUMENT && output.value.string.ok);
  output = (KuTaskAdapterOutcomeV1){0};
  CHECK(ku_task_3_take(&handle, NULL) == KU_TASK_CONTROL_INVALID_ARGUMENT);
  CHECK(ku_task_3_take(&handle, (KuTaskAdapterOutcomeV1*)&instance->payload) == KU_TASK_CONTROL_INVALID_ARGUMENT);
  CHECK(ku_task_3_take(&handle, (KuTaskAdapterOutcomeV1*)&instance->frame) == KU_TASK_CONTROL_INVALID_ARGUMENT);
  CHECK(ku_task_3_take(&handle, (KuTaskAdapterOutcomeV1*)&handle.ticket) == KU_TASK_CONTROL_INVALID_ARGUMENT);
  CHECK(ku_task_3_take(&handle, (KuTaskAdapterOutcomeV1*)&runtime.driver) == KU_TASK_CONTROL_INVALID_ARGUMENT);
  CHECK(ku_task_3_take(&handle, (KuTaskAdapterOutcomeV1*)runtime.slots) == KU_TASK_CONTROL_INVALID_ARGUMENT);
  /* Real owned backing is large enough for a typed output, but writing its
   * own owning Result there would leave a self-owned/dangling output header. */
  CHECK(ku_task_3_take(&handle, (KuTaskAdapterOutcomeV1*)original.ptr) == KU_TASK_CONTROL_INVALID_ARGUMENT);
  union { long double alignment; uint8_t bytes[256]; } unaligned = {0};
  CHECK(ku_task_3_take(&handle, (KuTaskAdapterOutcomeV1*)(unaligned.bytes + 1)) == KU_TASK_CONTROL_INVALID_ARGUMENT);
  CHECK(ku_task_3_take(&handle, &output) == KU_TASK_CONTROL_OK && output.value.string.ok && fixture_same_string(output.value.string.value, original));
  CHECK(ku_task_3_drop(&handle, fixture_deadline()) == KU_TASK_DRIVER_OK);
  fixture_runtime_finish(&runtime); ku_string_drop(&second); ku_task_outcome_drop(&output); fixture_ledger_zero();
}
static void fixture_private_failure_rollback(void) {
  FixtureRuntime runtime; fixture_runtime_init(&runtime, sizeof(KuTaskInstance_3) + 64);
  for (unsigned fault = FIXTURE_CONTROL_FAIL; fault <= FIXTURE_COMMIT_FAIL; ++fault) {
    KuString input = fixture_owned("restore after move", 64), original = input;
    KuTaskHandle_3 handle = {0}; FixtureLedger before = fixture_ledger();
    fixture_fault = fault; fixture_fault_calls = fixture_private_polls = fixture_private_drops = 0;
    fixture_moved_input = &input;
    uint32_t started = ku_task_3_try_start(&runtime.driver, &input, &handle);
    CHECK(started != KU_TASK_DRIVER_OK && fixture_fault_calls == 1);
    CHECK(fixture_same_string(input, original) && !handle.owner.lease.control && !handle.ticket.driver);
    CHECK(fixture_private_polls == (fault == FIXTURE_CONTROL_FAIL ? 0u : 1u));
    CHECK(fixture_private_drops == fixture_private_polls);
    CHECK(fixture_ledger().attempts == before.attempts + 1);
    CHECK(fixture_ledger().allocations == before.allocations && fixture_ledger().bytes == before.bytes);
    fixture_fault = FIXTURE_NO_FAULT; fixture_moved_input = NULL;
    fixture_empty(&runtime);
    /* Reusing the restored input proves the failure path did not secretly
     * dispose it while restoring only a dangling header. */
    CHECK(ku_task_3_try_start(&runtime.driver, &input, &handle) == KU_TASK_DRIVER_OK);
    fixture_idle(&runtime); KuTaskAdapterOutcomeV1 output = {0};
    CHECK(ku_task_3_take(&handle, &output) == KU_TASK_CONTROL_OK);
    CHECK(output.value.string.ok && fixture_same_string(output.value.string.value, original));
    CHECK(memcmp(output.value.string.value.ptr, "restore after move", output.value.string.value.len) == 0);
    CHECK(ku_task_3_drop(&handle, fixture_deadline()) == KU_TASK_DRIVER_OK);
    fixture_empty(&runtime); ku_task_outcome_drop(&output); fixture_ledger_zero();
  }
  fixture_runtime_finish(&runtime);
}
static void fixture_move_and_late_lease(void) {
  FixtureRuntime runtime; fixture_runtime_init(&runtime, sizeof(KuTaskInstance_3) + 64);
  union { long double alignment; uint8_t bytes[sizeof(KuTaskHandle_3) * 3]; } storage = {0};
  KuTaskHandle_3* handle = (KuTaskHandle_3*)storage.bytes;
  KuString input = fixture_owned("untaken", 64);
  CHECK(ku_task_3_try_start(&runtime.driver, &input, handle) == KU_TASK_DRIVER_OK);
  fixture_idle(&runtime);
  KuTaskControlV1* control = handle->owner.lease.control;
  size_t references = ku_task_control_atomic_load(&control->references);
  /* Full handles overlap even though the nested owner tokens do not. */
  KuTaskHandle_3* overlap = (KuTaskHandle_3*)(storage.bytes + sizeof(KuTaskControlOwnerV1));
  CHECK(ku_task_3_move(overlap, handle) == KU_TASK_CONTROL_INVALID_ARGUMENT);
  CHECK(handle->owner.lease.control == control && ku_task_control_atomic_load(&control->references) == references);
  KuTaskHandle_3 moved = {0};
  CHECK(ku_task_3_move(&moved, handle) == KU_TASK_CONTROL_OK);
  CHECK(!handle->owner.lease.control && !handle->ticket.driver && moved.owner.lease.control == control);
  CHECK(ku_task_control_atomic_load(&control->references) == references);
  KuTaskControlLeaseV1 late = {0};
  CHECK(ku_task_control_lease_retain(&moved.owner.lease, &late) == KU_TASK_CONTROL_OK);
  CHECK(ku_task_3_drop(&moved, fixture_deadline()) == KU_TASK_DRIVER_OK);
  fixture_idle(&runtime);
  KuTaskDriverSnapshotV1 retained = fixture_snapshot(&runtime);
  CHECK(retained.resident == 1 && retained.retiring == 1 && retained.reserved_bytes == sizeof(KuTaskInstance_3) + 64);
  CHECK(fixture_ledger().allocations == 1 && fixture_ledger().bytes == sizeof(KuTaskInstance_3));
  KuTaskAdapterOutcomeV1 output = {0}; KuTaskAdapterTakeRequestV1 request = {&output, NULL};
  CHECK(ku_task_control_take_result(&late, &request) == KU_TASK_CONTROL_RESULT_TAKEN);
  CHECK(ku_task_control_lease_release(&late) == KU_TASK_CONTROL_OK);
  fixture_runtime_finish(&runtime); fixture_ledger_zero();
}
static void fixture_sparse_postmove_rollback(void) {
  FixtureRuntime runtime; fixture_runtime_init(&runtime, sizeof(KuTaskInstance_8) + 256);
  for (unsigned error = 0; error < 2; ++error) {
    KuResult_str input = {0}; input.ok = !error;
    if (error) {
      input.error = fixture_error();
      input.value = (KuString){ (uint8_t*)(uintptr_t)1, SIZE_MAX, SIZE_MAX, 255 };
    } else {
      input.value = fixture_owned("restored sparse result", 64);
      input.error.message = (KuString){ (uint8_t*)(uintptr_t)1, SIZE_MAX, SIZE_MAX, 255 };
    }
    KuResult_str original = input;
    KuString cleanup = fixture_owned("restored sparse cleanup", 32), original_cleanup = cleanup;
    bool flag = true; int64_t number = 73; uint8_t nothing = 0;
    KuTaskHandle_8 handle = {0}; FixtureLedger before = fixture_ledger();
    fixture_fault = FIXTURE_COMMIT_FAIL; fixture_fault_calls = fixture_private_polls = fixture_private_drops = 0;
    fixture_moved_input = &cleanup; fixture_moved_result = &input;
    CHECK(ku_task_8_try_start(&runtime.driver, &input, &flag, &cleanup, &number, &nothing, &handle) == KU_TASK_DRIVER_INTERNAL);
    CHECK(fixture_fault_calls == 1 && fixture_private_polls == 1 && fixture_private_drops == 1);
    CHECK(fixture_same_result(input, original) && fixture_same_string(cleanup, original_cleanup));
    CHECK(flag && number == 73 && !nothing && !handle.owner.lease.control && !handle.ticket.driver);
    CHECK(fixture_ledger().attempts == before.attempts + 1);
    CHECK(fixture_ledger().allocations == before.allocations && fixture_ledger().bytes == before.bytes);
    fixture_fault = FIXTURE_NO_FAULT; fixture_moved_input = NULL; fixture_moved_result = NULL;
    fixture_empty(&runtime);
    CHECK(ku_task_8_try_start(&runtime.driver, &input, &flag, &cleanup, &number, &nothing, &handle) == KU_TASK_DRIVER_OK);
    CHECK(!input.value.ptr && !input.error.message.ptr && !cleanup.ptr && flag && number == 73 && !nothing);
    fixture_idle(&runtime); KuTaskAdapterOutcomeV1 output = {0};
    CHECK(ku_task_8_take(&handle, &output) == KU_TASK_CONTROL_OK && fixture_same_result(output.value.string, original));
    if (error) CHECK(memcmp(output.value.string.error.message.ptr, "message", 7) == 0);
    else CHECK(memcmp(output.value.string.value.ptr, "restored sparse result", output.value.string.value.len) == 0);
    CHECK(ku_task_8_drop(&handle, fixture_deadline()) == KU_TASK_DRIVER_OK);
    fixture_empty(&runtime); ku_task_outcome_drop(&output); fixture_ledger_zero();
  }
  fixture_runtime_finish(&runtime);
}
typedef struct FixtureStartWorker { FixtureRuntime* runtime; KuString input; KuTaskHandle_3 handle; uint32_t result; } FixtureStartWorker;
static int fixture_start_worker(void* raw) {
  FixtureStartWorker* worker = (FixtureStartWorker*)raw;
  worker->result = ku_task_3_try_start(&worker->runtime->driver, &worker->input, &worker->handle);
  return 0;
}
static void fixture_building_crosses_shutdown(bool clock_fault) {
  FixtureRuntime runtime; fixture_runtime_init(&runtime, sizeof(KuTaskInstance_3) + 64);
  fixture_events_init();
  FixtureStartWorker worker = {0}; worker.runtime = &runtime; worker.input = fixture_owned("accepted then cancelled", 64);
  fixture_fault = FIXTURE_COMMIT_GATE; fixture_fault_calls = 0; fixture_moved_input = &worker.input;
  KuTestThread thread; CHECK(ku_test_thread_start(&thread, fixture_start_worker, &worker));
  CHECK(ku_test_event_wait(&fixture_entered, 2000));
  KuTaskDriverSnapshotV1 building = fixture_snapshot(&runtime);
  CHECK(building.resident == 1 && building.building == 1 && building.reserved_bytes == sizeof(KuTaskInstance_3) + 64);
  if (clock_fault) {
    /* Exercise the real generated adapter clock bridge, not fabricated driver
     * fields. Its injected failed sample closes this BUILDING runtime. Unlike
     * an OS-specific clock fault, this portable bridge contract runs on all OS. */
    KuTaskAdapterClockV1 bridge = { &runtime.driver, NULL };
    fixture_clock_gate = 2;
    CHECK(ku_task_adapter_now(&bridge) == UINT64_MAX);
    KuTaskDriverSnapshotV1 failed = fixture_fault_idle(&runtime, false);
    CHECK(failed.resident == 1 && failed.building == 1);
    CHECK(ku_task_driver_shutdown(&runtime.driver, fixture_deadline()) == KU_TASK_DRIVER_INTERNAL);
  } else {
    CHECK(ku_task_driver_shutdown(&runtime.driver, ku_task_driver_now_ms() + 20) == KU_TASK_DRIVER_SHUTDOWN_TIMEOUT);
  }
  CHECK(ku_task_driver_destroy(&runtime.driver) == KU_TASK_DRIVER_PENDING);
  CHECK(ku_test_event_set(&fixture_proceed));
  CHECK(ku_test_thread_join(&thread, 2000) && thread.outcome == 0);
  fixture_fault = FIXTURE_NO_FAULT; fixture_moved_input = NULL;
  CHECK(worker.result == KU_TASK_DRIVER_OK && !worker.input.ptr && worker.handle.owner.lease.control);
  if (clock_fault) {
    KuTaskDriverSnapshotV1 held = fixture_fault_idle(&runtime, false);
    CHECK(held.resident == 1 && held.terminal_held == 1 && !held.building);
    CHECK(ku_task_control_cleanup_deadline(worker.handle.owner.lease.control) == 0);
  } else fixture_idle(&runtime);
  CHECK(ku_task_control_status(&worker.handle.owner.lease) == KU_TASK_CONTROL_CANCELLED);
  CHECK(ku_task_3_drop(&worker.handle, fixture_deadline()) == KU_TASK_DRIVER_OK);
  if (clock_fault) fixture_fault_finish(&runtime); else fixture_runtime_finish(&runtime);
  fixture_clock_gate = 0;
  fixture_events_finish(); fixture_ledger_zero();
}
static void fixture_yield_fairness(void) {
  FixtureRuntime runtime; fixture_runtime_init(&runtime, sizeof(KuTaskInstance_0) + sizeof(KuTaskInstance_1));
  fixture_events_init(); fixture_resume_gate = 1; fixture_resume_calls = fixture_other_resume_calls = 0;
  int64_t first = 10; bool second = true;
  KuTaskHandle_0 first_handle = {0}; KuTaskHandle_1 second_handle = {0};
  CHECK(ku_task_0_try_start(&runtime.driver, &first, &first_handle) == KU_TASK_DRIVER_OK);
  CHECK(ku_test_event_wait(&fixture_entered, 2000));
  CHECK(ku_task_1_try_start(&runtime.driver, &second, &second_handle) == KU_TASK_DRIVER_OK);
  CHECK(ku_test_event_set(&fixture_proceed)); fixture_idle(&runtime);
  CHECK(fixture_resume_calls == 2 && fixture_other_resume_calls == 2);
  fixture_resume_gate = 0;
  KuTaskAdapterOutcomeV1 first_output = {0}, second_output = {0};
  CHECK(ku_task_0_take(&first_handle, &first_output) == KU_TASK_CONTROL_OK);
  CHECK(ku_task_1_take(&second_handle, &second_output) == KU_TASK_CONTROL_OK);
  CHECK(first_output.value.integer.ok && first_output.value.integer.value == 10 && second_output.value.boolean.ok && second_output.value.boolean.value);
  CHECK(ku_task_0_drop(&first_handle, fixture_deadline()) == KU_TASK_DRIVER_OK);
  CHECK(ku_task_1_drop(&second_handle, fixture_deadline()) == KU_TASK_DRIVER_OK);
  fixture_runtime_finish(&runtime); ku_task_outcome_drop(&first_output); ku_task_outcome_drop(&second_output); fixture_events_finish(); fixture_ledger_zero();
}
static void fixture_private_ready_loses_cancellation(void) {
  FixtureRuntime runtime; fixture_runtime_init(&runtime, sizeof(KuTaskInstance_3) + 64);
  fixture_events_init(); fixture_resume_gate = 3; fixture_resume_calls = 0;
  KuString input = fixture_owned("ready but never published", 64);
  KuTaskHandle_3 handle = {0};
  CHECK(ku_task_3_try_start(&runtime.driver, &input, &handle) == KU_TASK_DRIVER_OK);
  CHECK(ku_test_event_wait(&fixture_entered, 2000));
  /* Original frame reached READY, but the generated adapter has not staged or
   * published it. Cancellation wins while its one real executor is suspended. */
  CHECK(ku_task_driver_request_cancel(&handle.ticket, &handle.owner.lease,
      KU_TASK_CONTROL_TIMED_OUT, fixture_deadline()) == KU_TASK_CONTROL_OK);
  CHECK(ku_test_event_set(&fixture_proceed)); fixture_idle(&runtime);
  CHECK(fixture_resume_calls == 1 && !input.ptr);
  CHECK(ku_task_control_status(&handle.owner.lease) == KU_TASK_CONTROL_TIMED_OUT);
  CHECK(fixture_ledger().allocations == 1 && fixture_ledger().bytes == sizeof(KuTaskInstance_3));
  CHECK(fixture_snapshot(&runtime).reserved_bytes == sizeof(KuTaskInstance_3) + 64);
  fixture_resume_gate = 0;
  KuTaskAdapterOutcomeV1 output = {0};
  CHECK(ku_task_3_take(&handle, &output) == KU_TASK_CONTROL_TIMED_OUT && !output.result_kind && !output.value.string.ok && !output.value.string.value.ptr);
  CHECK(ku_task_3_drop(&handle, fixture_deadline()) == KU_TASK_DRIVER_OK);
  fixture_runtime_finish(&runtime); fixture_events_finish(); fixture_ledger_zero();
}
static void fixture_live_deadline_tightening(bool tighten) {
  FixtureRuntime runtime; fixture_runtime_init(&runtime, sizeof(KuTaskInstance_10) + 128);
  fixture_events_init();
  fixture_resume_gate = 10; fixture_resume_calls = fixture_clock_calls = fixture_cleanup_expired = 0;
  KuResult_str input = {0}; input.ok = true; input.value = fixture_owned("owned result", 64);
  KuString cleanup = fixture_owned("owned cleanup", 64); bool loop = true;
  KuTaskHandle_10 handle = {0};
  CHECK(ku_task_10_try_start(&runtime.driver, &input, &cleanup, &loop, &handle) == KU_TASK_DRIVER_OK);
  CHECK(ku_test_event_wait(&fixture_entered, 2000));
  uint64_t original_deadline = fixture_deadline() + 1000000;
  CHECK(ku_task_driver_request_cancel(&handle.ticket, &handle.owner.lease,
      KU_TASK_CONTROL_TIMED_OUT, original_deadline) == KU_TASK_CONTROL_OK);
  fixture_clock_gate = 1;
  CHECK(ku_test_event_set(&fixture_proceed));
  CHECK(ku_test_event_wait(&fixture_clock_entered, 2000));
  if (tighten) CHECK(ku_task_driver_request_cancel(&handle.ticket, &handle.owner.lease,
      KU_TASK_CONTROL_CANCELLED, 0) == KU_TASK_CONTROL_OK);
  CHECK(ku_test_event_set(&fixture_clock_proceed)); fixture_idle(&runtime);
  CHECK(fixture_resume_calls == 1 && fixture_clock_calls >= 1 && fixture_clock_calls <= 8);
  CHECK(fixture_cleanup_initial_deadline == original_deadline && !!fixture_cleanup_expired == tighten);
  CHECK(ku_task_control_cleanup_deadline(handle.owner.lease.control) == (tighten ? 0 : original_deadline));
  CHECK(ku_task_control_status(&handle.owner.lease) == KU_TASK_CONTROL_TIMED_OUT);
  fixture_resume_gate = fixture_clock_gate = 0;
  KuTaskAdapterOutcomeV1 output = {0};
  CHECK(ku_task_10_take(&handle, &output) == KU_TASK_CONTROL_TIMED_OUT && !output.result_kind && !output.value.string.ok && !output.value.string.value.ptr);
  CHECK(ku_task_10_drop(&handle, fixture_deadline()) == KU_TASK_DRIVER_OK);
  fixture_runtime_finish(&runtime); fixture_events_finish(); fixture_ledger_zero();
}
int main(void) {
  CHECK(KU_TASK_FRAME_ABI_VERSION == 2u);
  fixture_primitives(); fixture_strings(); fixture_results();
  fixture_budget_and_oom(); fixture_sparse_and_preflight(); fixture_header_shape_and_empty_output();
  fixture_private_failure_rollback(); fixture_sparse_postmove_rollback(); fixture_move_and_late_lease();
  fixture_building_crosses_shutdown(false); fixture_building_crosses_shutdown(true);
  fixture_yield_fairness(); fixture_private_ready_loses_cancellation();
  fixture_live_deadline_tightening(false); fixture_live_deadline_tightening(true);
  puts("task-adapter-v1-ok"); return 0;
}
"#;
