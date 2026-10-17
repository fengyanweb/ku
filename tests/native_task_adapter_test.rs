//! R4 typed factories over verified frames, not source async or public Task syntax.

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
    tasks.functions.push(TaskFunction {
        id: TaskFunctionId(11),
        name: "OwnedErrorProjections".into(),
        slots: vec![
            slot(IrType::Str),
            slot(IrType::Named("__ku_error_type".into())),
            slot(IrType::Str),
            slot(result(IrType::Str)),
        ],
        parameters: vec![SlotId(0)],
        entry: StateId(0),
        result: result(IrType::Str),
        states: vec![TaskState {
            operations: vec![
                TaskOp::MakeError {
                    dst: SlotId(1),
                    message: SlotId(0),
                },
                TaskOp::ErrorField {
                    dst: SlotId(2),
                    src: SlotId(1),
                    field: "message".into(),
                },
                TaskOp::Drop { slot: SlotId(2) },
                TaskOp::ErrorField {
                    dst: SlotId(2),
                    src: SlotId(1),
                    field: "message".into(),
                },
                TaskOp::Drop { slot: SlotId(2) },
                TaskOp::ErrorField {
                    dst: SlotId(2),
                    src: SlotId(1),
                    field: "message".into(),
                },
                TaskOp::WrapOk {
                    dst: SlotId(3),
                    src: SlotId(2),
                },
            ],
            terminator: TaskTerminator::Exit { value: SlotId(3) },
        }],
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
        12,
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
  --fixture_fail_next_allocation; return !fixture_fail_next_allocation;
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
static void fixture_fail_allocation_after(int attempts) {
  if (attempts<1) abort();
  fixture_alloc_lock(); fixture_fail_next_allocation = attempts; fixture_alloc_unlock();
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
static uint32_t fixture_resume_eleven(void* storage, size_t bytes, uint32_t abi, const KuTaskFrameClockV1* clock) {
  if (fixture_resume_gate==11) {
    CHECK(ku_test_event_set(&fixture_entered));
    CHECK(ku_test_event_wait(&fixture_proceed,2000));
  }
  return ku_task_frame_11_resume(storage,bytes,abi,clock);
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
#define ku_task_frame_11_resume fixture_resume_eleven
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
#undef ku_task_frame_11_resume
#undef ku_task_frame_10_terminate
#undef ku_task_driver_now_ms
"#;

const C_MAIN: &str = r#"
typedef struct FixtureRuntime { KuTaskDriverV1 driver; KuTaskDriverSlotV1 slots[4]; size_t ring[4]; } FixtureRuntime;
static uint64_t fixture_deadline(void) { return ku_task_driver_now_ms() + 2000; }
static KuTaskDriverSnapshotV1 fixture_snapshot(FixtureRuntime* runtime) {
  KuTaskDriverSnapshotV1 snapshot;
  CHECK(ku_task_driver_snapshot(&runtime->driver, &snapshot) == KU_TASK_DRIVER_OK);
  CHECK(snapshot.worker_target==1u && snapshot.workers_created==1u);
  CHECK(!snapshot.fault && snapshot.resident <= 4 && snapshot.queued <= 4 && snapshot.running <= 1);
  return snapshot;
}
static void fixture_idle(FixtureRuntime* runtime) {
  CHECK(ku_task_driver_wait_idle(&runtime->driver, fixture_deadline()) == KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot = fixture_snapshot(runtime);
  CHECK(!snapshot.running && !snapshot.queued && (snapshot.workers_waiting + snapshot.workers_exited == snapshot.workers_created));
}
static void fixture_runtime_init(FixtureRuntime* runtime, size_t budget) {
  memset(runtime, 0, sizeof(*runtime));
  size_t fixed = sizeof(runtime->driver) + sizeof(runtime->slots) + sizeof(runtime->ring);
  CHECK(budget <= SIZE_MAX - fixed);
  uint64_t startup_now = ku_task_driver_now_ms();
  CHECK(startup_now <= UINT64_MAX - 1000u);
  uint64_t startup_deadline = startup_now + 1000u;
  CHECK(startup_deadline != UINT64_MAX);
  CHECK(ku_task_driver_init(&runtime->driver, sizeof(runtime->driver), KU_TASK_DRIVER_ABI_VERSION,
      runtime->slots, 4, runtime->ring, 4, fixed + budget, 1u, startup_deadline) == KU_TASK_DRIVER_OK);
  fixture_idle(runtime);
}
static void fixture_empty(FixtureRuntime* runtime) {
  fixture_idle(runtime);
  KuTaskDriverSnapshotV1 snapshot = fixture_snapshot(runtime);
  CHECK(!snapshot.resident && !snapshot.building && !snapshot.reserved_bytes);
}
static void fixture_reap(FixtureRuntime* runtime, uint64_t deadline, bool late) {
  KuTaskDriverSnapshotV1 snapshot = {0};
  CHECK(ku_task_driver_snapshot(&runtime->driver, &snapshot) == KU_TASK_DRIVER_OK);
  CHECK(snapshot.worker_target == 1u && snapshot.workers_created == 1u && snapshot.workers_exited == 1u);
  CHECK(!snapshot.workers_waiting && !snapshot.workers_joined && !snapshot.resident
      && !snapshot.building && !snapshot.queued && !snapshot.running && !snapshot.reserved_bytes);
#if defined(_WIN32)
  /* Only already-expired builder/clock-fault cases use the original 2-second
   * OS observation, after all EXITED and no Task storage. This is not a new
   * cleanup budget or evidence of cleanup within D (which may be zero). */
  if (late) CHECK(WaitForSingleObject(runtime->driver.workers[0].thread, 2000) == WAIT_OBJECT_0);
#else
  (void)late;
  alarm(2); /* Independent observer bound for the portable POSIX join tail. */
#endif
  CHECK(ku_task_driver_join(&runtime->driver, deadline) == KU_TASK_DRIVER_OK);
  CHECK(runtime->driver.workers_joined == 1u && runtime->driver.workers[0].joined && runtime->driver.workers[0].closed);
  CHECK(ku_task_driver_destroy(&runtime->driver) == KU_TASK_DRIVER_OK);
#if !defined(_WIN32)
  alarm(0);
#endif
}
static void fixture_runtime_finish(FixtureRuntime* runtime) {
  fixture_empty(runtime);
  CHECK(ku_task_driver_lock(&runtime->driver) == 0);
  bool was_closing = runtime->driver.closing != 0;
  uint64_t deadline = was_closing ? runtime->driver.shutdown_deadline : fixture_deadline();
  CHECK(ku_task_driver_unlock(&runtime->driver) == 0);
  CHECK(ku_task_driver_shutdown(&runtime->driver, deadline) == KU_TASK_DRIVER_OK);
  /* Shutdown applies its existing root cap once; join reuses that exact D. */
  CHECK(runtime->driver.shutdown_deadline <= deadline);
  fixture_reap(runtime, runtime->driver.shutdown_deadline, was_closing);
}
static KuTaskDriverSnapshotV1 fixture_fault_idle(FixtureRuntime* runtime, bool exited) {
  /* Public idle correctly reports the sticky fault immediately. Wait on the
   * actual protected worker predicate with the healthy test-caller clock. */
  CHECK(ku_task_driver_lock(&runtime->driver) == 0);
  uint64_t deadline = fixture_deadline();
  CHECK(runtime->driver.worker_target == 1u && runtime->driver.workers_created == 1u);
  while (runtime->driver.queued || runtime->driver.running
      || (exited ? runtime->driver.workers_exited != 1u : runtime->driver.workers_waiting != 1u)) {
    CHECK(ku_task_driver_wait(&runtime->driver, deadline) >= 0);
    CHECK(ku_task_driver_now_ms() < deadline);
  }
  CHECK(ku_task_driver_unlock(&runtime->driver) == 0);
  KuTaskDriverSnapshotV1 snapshot;
  CHECK(ku_task_driver_snapshot(&runtime->driver, &snapshot) == KU_TASK_DRIVER_OK);
  CHECK(snapshot.worker_target == 1u && snapshot.workers_created == 1u);
  CHECK(snapshot.fault == KU_TASK_DRIVER_INTERNAL && snapshot.clock_fault && snapshot.closing);
  CHECK(!snapshot.queued && !snapshot.running);
  return snapshot;
}
static void fixture_fault_finish(FixtureRuntime* runtime) {
  KuTaskDriverSnapshotV1 snapshot = fixture_fault_idle(runtime, true);
  CHECK(!snapshot.resident && !snapshot.reserved_bytes && !snapshot.building && snapshot.workers_exited == 1u);
  CHECK(runtime->driver.shutdown_deadline == 0);
  CHECK(ku_task_driver_shutdown(&runtime->driver, 0) == KU_TASK_DRIVER_INTERNAL);
  fixture_reap(runtime, 0, true);
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
static void fixture_owned_error_projection_budget(void) {
  for (int mode=0;mode<5;mode++) {
    const size_t capacity=64, length=6;
    FixtureRuntime runtime;
    fixture_runtime_init(&runtime,sizeof(KuTaskInstance_11)+capacity+(mode==1 ? 0 : length));
    fixture_events_init(); fixture_resume_gate=11;
    KuString input=fixture_owned("error!",capacity);
    KuTaskHandle_11 handle={0};
    CHECK(ku_task_11_try_start(&runtime.driver,&input,&handle)==KU_TASK_DRIVER_OK);
    CHECK(!input.ptr && ku_test_event_wait(&fixture_entered,2000));
    FixtureLedger before=fixture_ledger();
    KuTaskDriverSnapshotV1 held=fixture_snapshot(&runtime);
    CHECK(held.reserved_bytes==sizeof(KuTaskInstance_11)+capacity);
    CHECK(ku_task_driver_grow_charge(&handle.ticket,SIZE_MAX)==KU_TASK_DRIVER_LIMIT);
    CHECK(ku_task_driver_reconcile_charge(&handle.ticket,sizeof(KuTaskInstance_11),capacity+1)==KU_TASK_DRIVER_INVALID_STATE);
    KuTaskDriverTicketV1 stale=handle.ticket; stale.generation++;
    CHECK(ku_task_driver_grow_charge(&stale,1)==KU_TASK_DRIVER_STALE);
    CHECK(fixture_ledger().attempts==before.attempts);
    if (mode>=2) fixture_fail_allocation_after(mode-1);
    CHECK(ku_test_event_set(&fixture_proceed)); fixture_idle(&runtime);
    fixture_resume_gate=0;
    KuTaskDriverSnapshotV1 done=fixture_snapshot(&runtime);
    CHECK(done.reserved_bytes==sizeof(KuTaskInstance_11)+(mode==0 ? length : 0));
    CHECK(done.peak_reserved_bytes==sizeof(KuTaskInstance_11)+capacity+(mode==1 ? 0 : length));
    CHECK(fixture_ledger().attempts==before.attempts+(mode==0 ? 3 : mode==1 ? 0 : (size_t)(mode-1)));
    CHECK(fixture_ledger().allocations==(mode==0 ? 2u : 1u));
    KuTaskAdapterOutcomeV1 output={0};
    CHECK(ku_task_11_take(&handle,&output)==KU_TASK_CONTROL_OK);
    CHECK(output.value.string.ok==(mode==0));
    if (!mode) {
      CHECK(output.value.string.value.len==length);
      CHECK(!memcmp(output.value.string.value.ptr,"error!",length));
    } else {
      KuString code=output.value.string.error.code;
      const char* expected=mode==1 ? "memory_limit" : "out_of_memory";
      CHECK(code.len==strlen(expected) && !memcmp(code.ptr,expected,code.len));
      CHECK(output.exit_class==KU_TASK_EXIT_RUNTIME_FAILURE);
    }
    CHECK(fixture_snapshot(&runtime).reserved_bytes==sizeof(KuTaskInstance_11));
    CHECK(ku_task_11_drop(&handle,fixture_deadline())==KU_TASK_DRIVER_OK);
    ku_task_outcome_drop(&output); fixture_runtime_finish(&runtime);
    fixture_events_finish(); fixture_ledger_zero();
  }
}
int main(void) {
  CHECK(KU_TASK_FRAME_ABI_VERSION == 5u && KU_TASK_DRIVER_ABI_VERSION == 10u);
  fixture_primitives(); fixture_strings(); fixture_results();
  fixture_budget_and_oom(); fixture_sparse_and_preflight(); fixture_header_shape_and_empty_output();
  fixture_private_failure_rollback(); fixture_sparse_postmove_rollback(); fixture_move_and_late_lease();
  fixture_building_crosses_shutdown(false); fixture_building_crosses_shutdown(true);
  fixture_yield_fairness(); fixture_private_ready_loses_cancellation();
  fixture_live_deadline_tightening(false); fixture_live_deadline_tightening(true);
  fixture_owned_error_projection_budget();
  puts("task-adapter-v1-ok"); return 0;
}
"#;

#[test]
fn native_task_finally_adapter_transfers_owners_before_body_and_never_replays_on_ack_retry() {
    let source = r#"
async fn Child(): null! {
    try { while (true) {} } finally { println("child") }
    return ok(null)
}
async fn Parent(value: str): null! {
    child = Child()
    try { while (true) {} } finally { println(value) }
    return ok(null)
}
async fn main(): null! { return ok(null) }
"#;
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let native = ku::ir::task_lower::lower_program(&ast).unwrap();
    let generated = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
    let hooks = format!(
        "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{}\n{ALLOCATION_HOOK}\n{}\nstatic void fixture_finally_resumed(unsigned,void*,uint32_t);\nstatic uint32_t fixture_finally_child_gate(void*);\nstatic void fixture_finally_parent_cleanup(void*);\nstatic size_t fixture_finally_adapter_write(const void*,size_t,size_t,FILE*);\n#define fwrite fixture_finally_adapter_write\n",
        native_task_ledger::LEDGER_LOCK, native_task_ledger::LOCKED_ALLOCATIONS
    );
    let mut generated = generated.replacen(
        "typedef struct KuString {",
        &format!("{hooks}typedef struct KuString {{"),
        1,
    );
    for id in 0..2 {
        let call = format!("status = ku_task_frame_{id}_resume(&instance->frame, sizeof(instance->frame), KU_TASK_FRAME_ABI_VERSION, &clock);");
        assert_eq!(generated.matches(&call).count(), 1);
        generated = generated.replacen(
            &call,
            &format!("{call}\n  fixture_finally_resumed({id}u,instance,status);"),
            1,
        );
    }
    let child_cleanup =
        "static uint32_t ku_task_0_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {";
    assert_eq!(generated.matches(child_cleanup).count(), 1);
    generated = generated.replacen(child_cleanup, &format!("{child_cleanup}\n  uint32_t gated=fixture_finally_child_gate(raw); if (gated!=UINT32_MAX) return gated;"), 1);
    let parent_cleanup =
        "static uint32_t ku_task_1_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {";
    assert_eq!(generated.matches(parent_cleanup).count(), 1);
    generated = generated.replacen(
        parent_cleanup,
        &format!("{parent_cleanup}\n  fixture_finally_parent_cleanup(raw);"),
        1,
    );
    generated = generated.replacen(
        "int main(void) {",
        "static int fixture_generated_finally_adapter_main(void) {",
        1,
    );
    let plan = verify_and_plan(&native.tasks, TaskLimits::default()).unwrap();
    generated.push_str(&FINALLY_ADAPTER_ACK_MAIN.replace(
        "@TASK_MASK@",
        &plan.functions[1].scope_task_mask.to_string(),
    ));
    let directory = TempDir::new("native-task-finally-ack");
    let path = directory.path().join("program.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must run finally adapter ownership"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("finally adapter ACK fixture stays under its process watchdog");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "parent\nchild\nparent\nchild\nfinally-adapter-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const FINALLY_ADAPTER_ACK_MAIN: &str = r#"
#undef fwrite
static KuTestEvent fixture_finally_parent_ready,fixture_finally_child_ready,fixture_finally_child_waiting,fixture_finally_retried;
static KuAtomicRefcount fixture_finally_release,fixture_finally_parent_calls,fixture_finally_parent_bodies,fixture_finally_child_bodies;
static KuTaskInstance_1* fixture_finally_parent;
static void fixture_finally_resumed(unsigned id,void* raw,uint32_t status) {
  if (status!=KU_TASK_FRAME_PENDING) return;
  if (id==0u) {
    KuTaskInstance_0* child=(KuTaskInstance_0*)raw;
    if (child->frame.header.cleanup_state!=UINT32_MAX) CHECK(ku_test_event_set(&fixture_finally_child_ready));
  } else {
    KuTaskInstance_1* parent=(KuTaskInstance_1*)raw;
    fixture_finally_parent=parent;
    if (parent->frame.header.cleanup_state!=UINT32_MAX) CHECK(ku_test_event_set(&fixture_finally_parent_ready));
  }
}
static uint32_t fixture_finally_child_gate(void* raw) {
  if (ku_task_control_atomic_load(&fixture_finally_release)) return UINT32_MAX;
  KuTaskInstance_0* child=(KuTaskInstance_0*)raw;
  CHECK(ku_test_event_set(&fixture_finally_child_waiting));
  CHECK(ku_task_driver_set_intent(&child->ticket,KU_TASK_DRIVER_YIELD)==KU_TASK_DRIVER_OK);
  return KU_TASK_CONTROL_PENDING; /* Delay a genuine child callback, never forge ACK. */
}
static void fixture_finally_parent_cleanup(void* raw) {
  KuTaskInstance_1* parent=(KuTaskInstance_1*)raw;
  size_t prior=ku_task_control_atomic_load(&fixture_finally_parent_calls);
  ku_task_control_atomic_store(&fixture_finally_parent_calls,prior+1u);
  if (prior) {
    CHECK(parent->values_cleaned && !parent->frame.header.initialized);
    CHECK(ku_task_control_atomic_load(&fixture_finally_parent_bodies)==1u);
    CHECK(ku_test_event_set(&fixture_finally_retried));
  }
}
static size_t fixture_finally_adapter_write(const void* bytes,size_t size,size_t count,FILE* stream) {
  if (size==1u && count==6u && !memcmp(bytes,"parent",6u)) {
    CHECK(fixture_finally_parent && !(fixture_finally_parent->frame.header.initialized & UINT64_C(@TASK_MASK@)) && fixture_finally_parent->receipt_mask);
    CHECK(!ku_task_control_atomic_load(&fixture_finally_parent_bodies));
    CHECK(!ku_task_control_atomic_load(&fixture_finally_child_bodies));
    ku_task_control_atomic_store(&fixture_finally_parent_bodies,1u);
  } else if (size==1u && count==5u && !memcmp(bytes,"child",5u)) {
    CHECK(ku_task_control_atomic_load(&fixture_finally_parent_bodies)==1u);
    CHECK(ku_task_control_atomic_load(&fixture_finally_release));
    CHECK(!ku_task_control_atomic_load(&fixture_finally_child_bodies));
    ku_task_control_atomic_store(&fixture_finally_child_bodies,1u);
  }
  return fwrite(bytes,size,count,stream);
}
static void fixture_finally_ack_case(uint32_t reason) {
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes);
  CHECK(ku_test_event_init(&fixture_finally_parent_ready)); CHECK(ku_test_event_init(&fixture_finally_child_ready));
  CHECK(ku_test_event_init(&fixture_finally_child_waiting)); CHECK(ku_test_event_init(&fixture_finally_retried));
  ku_task_control_atomic_init(&fixture_finally_release,0); ku_task_control_atomic_init(&fixture_finally_parent_calls,0);
  ku_task_control_atomic_init(&fixture_finally_parent_bodies,0); ku_task_control_atomic_init(&fixture_finally_child_bodies,0);
  KuTaskDriverV1* driver=(KuTaskDriverV1*)calloc(1,sizeof(*driver));
  KuTaskDriverSlotV1* slots=(KuTaskDriverSlotV1*)calloc(3u,sizeof(*slots));
  size_t* ring=(size_t*)calloc(3u,sizeof(*ring)); CHECK(driver && slots && ring);
  uint64_t now=ku_task_driver_now_ms(); CHECK(now!=UINT64_MAX && now<UINT64_MAX-10000u);
  uint64_t deadline=now+10000u;
  CHECK(ku_task_driver_init(driver,sizeof(*driver),KU_TASK_DRIVER_ABI_VERSION,slots,3u,ring,3u,1024u*1024u,1u,deadline)==KU_TASK_DRIVER_OK);
  KuString input={0}; input.ptr=(uint8_t*)malloc(31u); CHECK(input.ptr); memcpy((void*)input.ptr,"parent",6u);
  input.len=6u; input.capacity=31u; input.storage=KU_STRING_OWNED;
  KuTaskValueV1 root={0}; CHECK(ku_task_1_start_value(driver,&input,&root)==KU_TASK_DRIVER_OK && root.tag==KU_TASK_VALUE_LIVE && !input.ptr);
  CHECK(ku_test_event_wait(&fixture_finally_parent_ready,2000)); CHECK(ku_test_event_wait(&fixture_finally_child_ready,2000));
  CHECK(ku_task_driver_request_cancel(&root.ticket,&root.owner.lease,reason,deadline)==KU_TASK_CONTROL_OK);
  CHECK(ku_test_event_wait(&fixture_finally_child_waiting,2000));
  KuTaskInstance_1* parent=(KuTaskInstance_1*)root.owner.lease.control;
  CHECK(ku_task_control_atomic_load(&fixture_finally_parent_bodies)==1u);
  CHECK(parent->values_cleaned && !parent->frame.header.initialized && parent->receipt_mask);
  uint32_t other=reason==KU_TASK_CONTROL_CANCELLED ? KU_TASK_CONTROL_TIMED_OUT : KU_TASK_CONTROL_CANCELLED;
  CHECK(ku_task_driver_request_cancel(&root.ticket,&root.owner.lease,other,deadline-1000u)==KU_TASK_CONTROL_OK);
  CHECK(ku_test_event_wait(&fixture_finally_retried,2000));
  CHECK(ku_task_control_atomic_load(&fixture_finally_parent_bodies)==1u && !ku_task_control_atomic_load(&fixture_finally_child_bodies));
  ku_task_control_atomic_store(&fixture_finally_release,1u);
  CHECK(ku_task_driver_wait_idle(driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_control_atomic_load(&parent->control.phase)==reason);
  CHECK(ku_task_control_cleanup_deadline(&parent->control)==deadline-1000u);
  CHECK(ku_task_control_atomic_load(&fixture_finally_parent_bodies)==1u && ku_task_control_atomic_load(&fixture_finally_child_bodies)==1u);
  KuTaskAdapterOutcomeV1 outcome={0}; CHECK(ku_task_value_take(&root,NULL,&outcome)==reason && ku_task_outcome_empty(&outcome,3u));
  CHECK(ku_task_value_drop(&root,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_wait_idle(driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_shutdown(driver,deadline)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot={0}; CHECK(ku_task_driver_snapshot(driver,&snapshot)==KU_TASK_DRIVER_OK);
  CHECK(!snapshot.fault && !snapshot.resident && !snapshot.reserved_bytes && !snapshot.running && !snapshot.queued && !snapshot.building);
  CHECK(ku_task_driver_join(driver,deadline)==KU_TASK_DRIVER_OK); CHECK(ku_task_driver_destroy(driver)==KU_TASK_DRIVER_OK);
  free(ring); free(slots); free(driver);
  CHECK(ku_test_event_destroy(&fixture_finally_parent_ready)); CHECK(ku_test_event_destroy(&fixture_finally_child_ready));
  CHECK(ku_test_event_destroy(&fixture_finally_child_waiting)); CHECK(ku_test_event_destroy(&fixture_finally_retried));
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes && !fixture_ledger().overflow);
}
int main(void) {
  fixture_finally_ack_case(KU_TASK_CONTROL_CANCELLED); fixture_finally_ack_case(KU_TASK_CONTROL_TIMED_OUT);
  puts("finally-adapter-ok"); return 0;
}
"#;

fn finally_adapter_generated(source: &str, declarations: &str) -> String {
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let native = ku::ir::task_lower::lower_program(&ast).unwrap();
    let generated = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
    let hooks = format!(
        "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{}\n{ALLOCATION_HOOK}\n{}\n{declarations}",
        native_task_ledger::LEDGER_LOCK,
        native_task_ledger::LOCKED_ALLOCATIONS
    );
    assert_eq!(generated.matches("typedef struct KuString {").count(), 1);
    let generated = generated.replacen(
        "typedef struct KuString {",
        &format!("{hooks}typedef struct KuString {{"),
        1,
    );
    assert_eq!(generated.matches("int main(void) {").count(), 1);
    generated.replacen(
        "int main(void) {",
        "static int fixture_unmodified_finally_main(void) {",
        1,
    )
}

fn run_finally_adapter_generated(generated: String, label: &str, expected: &str) {
    let directory = TempDir::new(label);
    let path = directory.path().join("program.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must run finally adapter regression"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("finally adapter regression stays under its original process watchdog");
    assert!(
        output.status.success(),
        "{label}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        expected,
        "{label}"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn native_task_finally_adapter_staged_cleanup_observes_a_later_control_deadline() {
    let source = r#"
async fn Held(): null! { return ok(null) }
async fn Leaf(): null! { held = Held() print("io-start") return ok(null) }
async fn Parent(value: str): null! {
    try { (await Leaf())? } finally {
        println("protected-first")
        if (true) { println(value) }
    }
    return ok(null)
}
async fn main(): null! { return ok(null) }
"#;
    let declarations = "static void fixture_finally_deadline_enter(void*);\nstatic size_t fixture_finally_deadline_write(const void*,size_t,size_t,FILE*);\n#define fwrite fixture_finally_deadline_write\n";
    let mut generated = finally_adapter_generated(source, declarations);
    let now = "static uint64_t ku_task_driver_now_ms(void) {";
    assert_eq!(generated.matches(now).count(), 1);
    generated = generated.replacen(now, "static uint64_t ku_task_driver_now_ms(void);\nstatic uint64_t fixture_finally_real_now(void) {", 1);
    let resume = "static uint32_t ku_task_2_resume(void* raw) {";
    assert_eq!(generated.matches(resume).count(), 1);
    generated = generated.replacen(
        resume,
        &format!("{resume}\n  fixture_finally_deadline_enter(raw);"),
        1,
    );
    generated.push_str(FINALLY_ADAPTER_DEADLINE_MAIN);
    run_finally_adapter_generated(
        generated,
        "native-task-finally-live-deadline",
        "protected-first\nfinally-deadline-ok\n",
    );
}

const FINALLY_ADAPTER_DEADLINE_MAIN: &str = r#"
#undef fwrite
static KuTaskInstance_2* fixture_finally_deadline_parent;
static unsigned fixture_finally_io_failures,fixture_finally_first_writes,fixture_finally_late_writes;
/* Logical time is fixed, while the existing process watchdog uses real time. */
static uint64_t ku_task_driver_now_ms(void) { return 100u; }
static void fixture_finally_deadline_enter(void* raw) { fixture_finally_deadline_parent=(KuTaskInstance_2*)raw; }
static size_t fixture_finally_deadline_write(const void* bytes,size_t size,size_t count,FILE* stream) {
  if (size==1u && count==8u && !memcmp(bytes,"io-start",8u)) { fixture_finally_io_failures++; return 0; }
  if (size==1u && count==15u && !memcmp(bytes,"protected-first",15u)) {
    KuTaskInstance_2* parent=fixture_finally_deadline_parent;
    CHECK(parent && !fixture_finally_first_writes++);
    CHECK(parent->frame.header.status==KU_TASK_FRAME_EXIT_STAGED && parent->frame.header.has_exit_deadline);
    CHECK(parent->frame.header.exit_class==KU_TASK_EXIT_RUNTIME_FAILURE && parent->frame.header.exit_deadline==1100u);
    CHECK(ku_task_driver_cancel_bound(&parent->ticket,KU_TASK_CONTROL_CANCELLED,50u)==KU_TASK_CONTROL_OK);
  }
  if (size==1u && count==4u && !memcmp(bytes,"late",4u)) fixture_finally_late_writes++;
  return fwrite(bytes,size,count,stream);
}
int main(void) {
  KuTaskDriverV1* driver=(KuTaskDriverV1*)calloc(1,sizeof(*driver));
  KuTaskDriverSlotV1* slots=(KuTaskDriverSlotV1*)calloc(4u,sizeof(*slots));
  size_t* ring=(size_t*)calloc(4u,sizeof(*ring)); CHECK(driver && slots && ring);
  CHECK(ku_task_driver_init(driver,sizeof(*driver),KU_TASK_DRIVER_ABI_VERSION,slots,4u,ring,4u,1024u*1024u,1u,7000u)==KU_TASK_DRIVER_OK);
  uint8_t* bytes=(uint8_t*)malloc(31u); CHECK(bytes); memcpy(bytes,"late",4u);
  KuString value={bytes,4u,31u,KU_STRING_OWNED}; KuTaskValueV1 root={0};
  CHECK(ku_task_2_start_value(driver,&value,&root)==KU_TASK_DRIVER_OK && root.tag==KU_TASK_VALUE_LIVE && !value.ptr);
  CHECK(ku_task_driver_wait_idle(driver,7000u)==KU_TASK_DRIVER_OK);
  CHECK(fixture_finally_io_failures==1u && fixture_finally_first_writes==1u && !fixture_finally_late_writes);
  CHECK(ku_task_control_atomic_load(&root.owner.lease.control->phase)==KU_TASK_CONTROL_CANCELLED);
  KuTaskAdapterOutcomeV1 outcome={0}; CHECK(ku_task_value_take(&root,NULL,&outcome)==KU_TASK_CONTROL_CANCELLED && ku_task_outcome_empty(&outcome,3u));
  CHECK(ku_task_value_drop(&root,7000u)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_wait_idle(driver,7000u)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_shutdown(driver,7000u)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot={0}; CHECK(ku_task_driver_snapshot(driver,&snapshot)==KU_TASK_DRIVER_OK);
  CHECK(!snapshot.fault && !snapshot.resident && !snapshot.reserved_bytes && !snapshot.running && !snapshot.queued && !snapshot.building);
  CHECK(ku_task_driver_join(driver,7000u)==KU_TASK_DRIVER_OK); CHECK(ku_task_driver_destroy(driver)==KU_TASK_DRIVER_OK);
  free(ring); free(slots); free(driver);
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes && !fixture_ledger().overflow);
  puts("finally-deadline-ok"); return 0;
}
"#;

#[test]
fn native_task_finally_adapter_returns_try_result_owned_bytes_before_outer_projection() {
    let source = r#"
async fn Probe(saved: str!, value: str): null! {
    try { try { while (true) {} } finally { println(saved?) } }
    finally { try { fail value } catch (error) { println(error.message) } }
    return ok(null)
}
async fn main(): null! { return ok(null) }
"#;
    let declarations = "static void fixture_finally_budget_resumed(void*,uint32_t);\nstatic size_t fixture_finally_budget_write(const void*,size_t,size_t,FILE*);\n#define fwrite fixture_finally_budget_write\n";
    let mut generated = finally_adapter_generated(source, declarations);
    let call = "status = ku_task_frame_0_resume(&instance->frame, sizeof(instance->frame), KU_TASK_FRAME_ABI_VERSION, &clock);";
    assert_eq!(generated.matches(call).count(), 1);
    generated = generated.replacen(
        call,
        &format!("{call}\n  fixture_finally_budget_resumed(instance,status);"),
        1,
    );
    generated.push_str(FINALLY_ADAPTER_BUDGET_MAIN);
    run_finally_adapter_generated(
        generated,
        "native-task-finally-owned-refund",
        "outer\nouter\nfinally-owned-budget-ok\n",
    );
}

const FINALLY_ADAPTER_BUDGET_MAIN: &str = r#"
#undef fwrite
static KuTestEvent fixture_finally_budget_ready;
static KuTaskInstance_0* fixture_finally_budget_parent;
static unsigned fixture_finally_retained_failures,fixture_finally_outer_writes;
static void fixture_finally_budget_resumed(void* raw,uint32_t status) {
  KuTaskInstance_0* parent=(KuTaskInstance_0*)raw;
  fixture_finally_budget_parent=parent;
  if (status==KU_TASK_FRAME_PENDING && parent->frame.header.cleanup_state!=UINT32_MAX)
    CHECK(ku_test_event_set(&fixture_finally_budget_ready));
}
static size_t fixture_finally_budget_write(const void* bytes,size_t size,size_t count,FILE* stream) {
  if (size==1u && count==6u && !memcmp(bytes,"retain",6u)) { fixture_finally_retained_failures++; return 0; }
  if (size==1u && count==5u && !memcmp(bytes,"outer",5u)) {
    CHECK(fixture_finally_retained_failures==1u && !fixture_finally_outer_writes++);
    KuTaskDriverV1* driver=fixture_finally_budget_parent->ticket.driver;
    KuTaskDriverSnapshotV1 snapshot={0}; CHECK(ku_task_driver_snapshot(driver,&snapshot)==KU_TASK_DRIVER_OK);
    CHECK(snapshot.reserved_bytes==sizeof(KuTaskInstance_0)+29u+5u);
    CHECK(snapshot.byte_limit==snapshot.fixed_bytes+sizeof(KuTaskInstance_0)+31u+29u);
    CHECK(fixture_ledger().bytes==snapshot.fixed_bytes+sizeof(KuTaskInstance_0)+29u+5u);
  }
  return fwrite(bytes,size,count,stream);
}
static void fixture_finally_budget_case(uint32_t reason) {
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes);
  CHECK(ku_test_event_init(&fixture_finally_budget_ready));
  fixture_finally_retained_failures=fixture_finally_outer_writes=0;
  KuTaskDriverV1* driver=(KuTaskDriverV1*)calloc(1,sizeof(*driver));
  KuTaskDriverSlotV1* slots=(KuTaskDriverSlotV1*)calloc(1u,sizeof(*slots));
  size_t* ring=(size_t*)calloc(1u,sizeof(*ring)); CHECK(driver && slots && ring);
  size_t fixed=sizeof(*driver)+sizeof(*slots)+sizeof(*ring);
  size_t limit=fixed+sizeof(KuTaskInstance_0)+31u+29u;
  uint64_t now=ku_task_driver_now_ms(); CHECK(now!=UINT64_MAX && now<UINT64_MAX-10000u);
  uint64_t deadline=now+10000u;
  CHECK(ku_task_driver_init(driver,sizeof(*driver),KU_TASK_DRIVER_ABI_VERSION,slots,1u,ring,1u,limit,1u,deadline)==KU_TASK_DRIVER_OK);
  uint8_t* retained=(uint8_t*)malloc(31u); uint8_t* text=(uint8_t*)malloc(29u); CHECK(retained && text);
  memcpy(retained,"retain",6u); memcpy(text,"outer",5u);
  KuResult_str saved={true,{retained,6u,31u,KU_STRING_OWNED},{0}};
  KuString value={text,5u,29u,KU_STRING_OWNED}; KuTaskValueV1 root={0};
  CHECK(ku_task_0_start_value(driver,&saved,&value,&root)==KU_TASK_DRIVER_OK && root.tag==KU_TASK_VALUE_LIVE && !saved.value.ptr && !value.ptr);
  CHECK(ku_test_event_wait(&fixture_finally_budget_ready,2000));
  CHECK(ku_task_driver_request_cancel(&root.ticket,&root.owner.lease,reason,deadline)==KU_TASK_CONTROL_OK);
  CHECK(ku_task_driver_wait_idle(driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(fixture_finally_retained_failures==1u && fixture_finally_outer_writes==1u);
  CHECK(ku_task_control_atomic_load(&root.owner.lease.control->phase)==reason);
  KuTaskAdapterOutcomeV1 outcome={0}; CHECK(ku_task_value_take(&root,NULL,&outcome)==reason && ku_task_outcome_empty(&outcome,3u));
  CHECK(ku_task_value_drop(&root,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_wait_idle(driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_shutdown(driver,deadline)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot={0}; CHECK(ku_task_driver_snapshot(driver,&snapshot)==KU_TASK_DRIVER_OK);
  CHECK(!snapshot.fault && !snapshot.resident && !snapshot.reserved_bytes && !snapshot.running && !snapshot.queued && !snapshot.building);
  CHECK(snapshot.peak_reserved_bytes==sizeof(KuTaskInstance_0)+31u+29u);
  CHECK(ku_task_driver_join(driver,deadline)==KU_TASK_DRIVER_OK); CHECK(ku_task_driver_destroy(driver)==KU_TASK_DRIVER_OK);
  free(ring); free(slots); free(driver); CHECK(ku_test_event_destroy(&fixture_finally_budget_ready));
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes && !fixture_ledger().overflow);
}
int main(void) {
  fixture_finally_budget_case(KU_TASK_CONTROL_CANCELLED); fixture_finally_budget_case(KU_TASK_CONTROL_TIMED_OUT);
  puts("finally-owned-budget-ok"); return 0;
}
"#;

#[test]
fn native_task_blocking_write_tracks_real_jobs_budget_and_original_cleanup_deadline() {
    let source = r#"
import fs from "std.fs"
import time from "std.time"
async fn Save(path: str, content: str): null! { fs.try_write(path, content)? return ok(null) }
async fn Tick(): null! { time.sleep(1)? return ok(null) }
async fn Load(path: str): str! { return fs.try_read(path) }
async fn main(): null! { return ok(null) }
"#;
    let mut ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    // Mirror the CLI's resolved standard-module capabilities. The direct
    // checker does not expand Import items into these Module declarations.
    ast.items
        .retain(|item| !matches!(item, ku::ast::Item::Import(_)));
    for name in ["std:fs", "std:time"] {
        ast.items.insert(
            0,
            ku::ast::Item::Module(ku::ast::ModuleDecl {
                name: name.into(),
                span: Default::default(),
            }),
        );
    }
    Checker::new().check(&ast).unwrap();
    let native = ir::task_lower::lower_program(&ast).unwrap();
    let generated = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
    let instrumentation = format!(
        "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{}\n{ALLOCATION_HOOK}\n{}\n{BLOCKING_FIXTURE_HOOKS}\n",
        native_task_ledger::LEDGER_LOCK,
        native_task_ledger::LOCKED_ALLOCATIONS
    );
    let mut generated = generated
        .replacen("typedef struct KuString {", &format!("{instrumentation}typedef struct KuString {{"), 1)
        .replacen("int main(void) {", "static int ku_generated_main(void) {", 1)
        .replacen(
            "static bool ku_fs_write_native_impl(const KuFsNativeChar* native, KuString content, KuError* error) {",
            "static bool ku_fs_write_native_impl(const KuFsNativeChar* native, KuString content, KuError* error) {\n  fixture_io_gate();",
            1,
        )
        .replacen(
            "static bool ku_fs_read_native_impl(const KuFsNativeChar* native, KuString* value, KuError* error, KuFsAllocatorV1* allocator) {",
            "static bool ku_fs_read_native_impl(const KuFsNativeChar* native, KuString* value, KuError* error, KuFsAllocatorV1* allocator) {\n  fixture_io_gate();",
            1,
        )
        .replacen("void* pointer=malloc(bytes);", "void* pointer=fixture_output_malloc(bytes);", 1)
        .replacen("job->state=KU_TASK_BLOCKING_DONE; pool->running--;", "job->state=KU_TASK_BLOCKING_DONE; pool->running--; fixture_blocking_done(job->kind);", 1)
        .replacen(
            "static void ku_task_blocking_signal(KuTaskBlockingPoolV1* pool) {",
            "static void ku_task_blocking_signal(KuTaskBlockingPoolV1* pool) {\n  if (fixture_pool_signal_failure()) { pool->fault=KU_TASK_DRIVER_INTERNAL; return; }",
            1,
        )
        .replacen(
            "KuTaskBlockingAllocationV1* allocation=(KuTaskBlockingAllocationV1*)malloc(total);",
            "KuTaskBlockingAllocationV1* allocation=(KuTaskBlockingAllocationV1*)fixture_blocking_malloc(total);",
            1,
        );
    generated.push_str(BLOCKING_FIXTURE_MAIN);
    let directory = TempDir::new("native-blocking-write-ledger");
    let path = directory.path().join("blocking.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "blocking") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires the native blocking gate"
        );
        return;
    };
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "blocking-write-ledger-ok"
    );
}

const BLOCKING_FIXTURE_HOOKS: &str = r#"
static KuTestEvent fixture_io_entered,fixture_io_release;
static int fixture_gate_io;
static size_t fixture_io_calls,fixture_fail_blocking_in=SIZE_MAX;
static void* fixture_original_path;
static void* fixture_original_content;
static size_t fixture_original_path_freed,fixture_original_content_freed;
static void fixture_original_free(void* pointer) {
  fixture_alloc_lock();
  if (pointer && pointer==fixture_original_path) fixture_original_path_freed++;
  if (pointer && pointer==fixture_original_content) fixture_original_content_freed++;
  fixture_alloc_unlock(); fixture_free(pointer);
}
#undef free
#define free fixture_original_free
static void fixture_blocking_done(unsigned kind);
static size_t fixture_fail_output_in=SIZE_MAX, fixture_output_calls;
static int fixture_gate_output;
static void* fixture_output_malloc(size_t bytes) {
  fixture_alloc_lock();
  fixture_output_calls++;
  int fail=fixture_fail_output_in==0;
  if (fixture_fail_output_in!=SIZE_MAX) {
    if (fail) fixture_fail_output_in=SIZE_MAX; else fixture_fail_output_in--;
  }
  int gate=fixture_gate_output && fixture_output_calls==1u;
  fixture_alloc_unlock();
  void* pointer=fail ? NULL : fixture_malloc(bytes);
  if (gate) { CHECK(ku_test_event_set(&fixture_io_entered)); CHECK(ku_test_event_wait(&fixture_io_release,2000)); }
  return pointer;
}
static int fixture_fail_pool_signal;
static int fixture_pool_signal_failure(void) {
  fixture_alloc_lock(); int fail=fixture_fail_pool_signal; fixture_fail_pool_signal=0;
  fixture_alloc_unlock(); return fail;
}
static void fixture_io_gate(void) {
  fixture_alloc_lock(); fixture_io_calls++; int gate=fixture_gate_io; fixture_alloc_unlock();
  if (gate) { CHECK(ku_test_event_set(&fixture_io_entered)); CHECK(ku_test_event_wait(&fixture_io_release,2000)); }
}
static void* fixture_blocking_malloc(size_t bytes) {
  fixture_alloc_lock();
  int fail=fixture_fail_blocking_in==0;
  if (fixture_fail_blocking_in!=SIZE_MAX) {
    if (fail) fixture_fail_blocking_in=SIZE_MAX; else fixture_fail_blocking_in--;
  }
  fixture_alloc_unlock(); return fail ? NULL : fixture_malloc(bytes);
}
"#;

const BLOCKING_FIXTURE_MAIN: &str = r#"
static KuTaskDriverV1 blocking_driver;
static KuTaskDriverSlotV1 blocking_slots[4];
static size_t blocking_ring[4];
static KuTaskBlockingPoolV1 blocking_pool;
static KuTaskControlLeaseV1 fixture_read_done_lease;
static uint64_t fixture_read_done_deadline;
static void fixture_blocking_done(unsigned kind) {
  if (kind==1u && fixture_read_done_lease.control)
    CHECK(ku_task_control_request_cancel(&fixture_read_done_lease,KU_TASK_CONTROL_CANCELLED,fixture_read_done_deadline)==KU_TASK_CONTROL_OK);
}
static uint64_t blocking_observation_deadline(void) {
  uint64_t now=ku_task_driver_now_ms(); CHECK(now!=UINT64_MAX); return now+2000u;
}
static void blocking_idle(void) {
  CHECK(ku_task_driver_wait_idle(&blocking_driver,blocking_observation_deadline())==KU_TASK_DRIVER_OK);
}
static KuTaskDriverSnapshotV1 blocking_driver_snapshot(void) {
  KuTaskDriverSnapshotV1 snapshot={0};
  CHECK(ku_task_driver_snapshot(&blocking_driver,&snapshot)==KU_TASK_DRIVER_OK); return snapshot;
}
static KuTaskBlockingSnapshotV1 blocking_pool_snapshot(void) {
  KuTaskBlockingSnapshotV1 snapshot={0};
  CHECK(ku_task_blocking_snapshot(&blocking_pool,&snapshot)==KU_TASK_DRIVER_OK); return snapshot;
}
static void blocking_setup(size_t capacity,size_t dynamic_budget) {
  memset(&blocking_driver,0,sizeof(blocking_driver)); memset(blocking_slots,0,sizeof(blocking_slots));
  memset(blocking_ring,0,sizeof(blocking_ring)); memset(&blocking_pool,0,sizeof(blocking_pool));
  fixture_gate_io=0; fixture_io_calls=0; fixture_fail_blocking_in=SIZE_MAX; fixture_fail_pool_signal=0;
  fixture_fail_output_in=SIZE_MAX; fixture_output_calls=0; fixture_gate_output=0;
  fixture_read_done_lease=(KuTaskControlLeaseV1){0}; fixture_read_done_deadline=0;
  fixture_original_path=NULL; fixture_original_content=NULL;
  fixture_original_path_freed=0; fixture_original_content_freed=0;
  CHECK(ku_test_event_init(&fixture_io_entered)); CHECK(ku_test_event_init(&fixture_io_release));
  size_t fixed=sizeof(blocking_driver)+sizeof(blocking_slots)+sizeof(blocking_ring)+sizeof(blocking_pool);
  uint64_t deadline=blocking_observation_deadline();
  CHECK(ku_task_driver_init(&blocking_driver,sizeof(blocking_driver),KU_TASK_DRIVER_ABI_VERSION,
      blocking_slots,4,blocking_ring,4,fixed+dynamic_budget,1,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_blocking_init(&blocking_pool,capacity,1,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_attach_blocking(&blocking_driver,&blocking_pool,sizeof(blocking_pool),
      ku_task_blocking_close,ku_task_blocking_ready)==KU_TASK_DRIVER_OK);
  CHECK(blocking_driver_snapshot().fixed_bytes==fixed);
  CHECK(!ku_task_driver_external_storage(&blocking_driver,&blocking_pool.jobs[0],sizeof(blocking_pool.jobs[0]),1));
  KuString alias=ku_string_static((const uint8_t*)&blocking_pool.jobs[0],1);
  CHECK(ku_task_adapter_string_overlaps_runtime(alias,&blocking_driver));
}
static void blocking_finish(uint64_t deadline) {
  CHECK(ku_task_driver_shutdown(&blocking_driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_blocking_finish(&blocking_pool,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_join(&blocking_driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_destroy(&blocking_driver)==KU_TASK_DRIVER_OK);
  CHECK(ku_test_event_destroy(&fixture_io_entered)); CHECK(ku_test_event_destroy(&fixture_io_release));
  FixtureLedger ledger=fixture_ledger(); CHECK(!ledger.allocations && !ledger.bytes && !ledger.overflow);
}
static KuTaskValueV1 blocking_start(const char* name) {
  KuString path=ku_string_static((const uint8_t*)name,strlen(name));
  KuString text=ku_string_static((const uint8_t*)"owned copy 世界",17);
  KuTaskValueV1 task={0};
  CHECK(ku_task_0_start_value(&blocking_driver,&path,&text,&task)==KU_TASK_DRIVER_OK);
  CHECK(task.tag==KU_TASK_VALUE_LIVE); return task;
}
static KuTaskValueV1 blocking_start_owned_late(void) {
  uint8_t* path=(uint8_t*)fixture_malloc(31u);
  uint8_t* content=(uint8_t*)fixture_malloc(37u); CHECK(path && content);
  memcpy(path,"late.txt",8u); memcpy(content,"owned copy 世界",17u);
  fixture_original_path=path; fixture_original_content=content;
  KuString path_value={path,8u,31u,KU_STRING_OWNED};
  KuString content_value={content,17u,37u,KU_STRING_OWNED};
  KuTaskValueV1 task={0};
  CHECK(ku_task_0_start_value(&blocking_driver,&path_value,&content_value,&task)==KU_TASK_DRIVER_OK);
  CHECK(task.tag==KU_TASK_VALUE_LIVE && !path_value.ptr && !content_value.ptr);
  return task;
}
static void blocking_take(KuTaskValueV1* task,const char* error_code) {
  CHECK(ku_task_driver_wait_result(&task->ticket,&task->owner.lease,blocking_observation_deadline())==KU_TASK_DRIVER_WAIT_READY);
  KuTaskAdapterOutcomeV1 output={0}; CHECK(ku_task_value_take(task,NULL,&output)==KU_TASK_CONTROL_OK);
  CHECK(output.result_kind==3u && output.value.null_value.ok==(error_code==NULL));
  if (error_code) {
    KuString code=output.value.null_value.error.code;
    CHECK(code.len==strlen(error_code) && !memcmp(code.ptr,error_code,code.len));
  }
  ku_task_outcome_drop(&output);
}
static void blocking_drop(KuTaskValueV1* task) {
  CHECK(ku_task_value_drop(task,blocking_observation_deadline())==KU_TASK_DRIVER_OK);
}
static void blocking_no_file(const char* name) {
  FILE* file=fopen(name,"rb"); CHECK(!file);
}
static void blocking_queue_and_generation(void) {
  blocking_setup(2,4u*(sizeof(KuTaskInstance_0)+sizeof(KuTaskInstance_1))+1024u);
  fixture_gate_io=1;
  KuTaskValueV1 first=blocking_start("first.txt"); CHECK(ku_test_event_wait(&fixture_io_entered,2000));
  KuTaskValueV1 queued=blocking_start("queued-cancelled.txt"); blocking_idle();
  KuTaskBlockingSnapshotV1 pending=blocking_pool_snapshot(); CHECK(pending.running==1 && pending.queued==1 && pending.resident==2);
  KuTaskInstance_0* instance=(KuTaskInstance_0*)first.owner.lease.control->context;
  KuTaskBlockingTokenV1 stale=instance->blocking; KuTaskDriverTicketV1 old_ticket=first.ticket;
  CHECK(stale.pool && stale.charged_bytes && blocking_driver_snapshot().reserved_bytes>2u*sizeof(KuTaskInstance_0));
  KuTaskValueV1 full=blocking_start("queue-full.txt"); blocking_take(&full,"blocking_queue_full"); blocking_drop(&full);
  KuTaskValueV1 timer={0}; CHECK(ku_task_1_start_value(&blocking_driver,&timer)==KU_TASK_DRIVER_OK);
  blocking_take(&timer,NULL); blocking_drop(&timer);
  // Publish cancellation without a parent poll. The queued worker must acquire
  // the control phase before its own file-operation start decision.
  uint64_t deadline=blocking_observation_deadline();
  CHECK(ku_task_control_request_cancel(&queued.owner.lease,KU_TASK_CONTROL_CANCELLED,deadline)==KU_TASK_CONTROL_OK);
  CHECK(ku_test_event_set(&fixture_io_release)); blocking_take(&first,NULL);
  CHECK(ku_task_driver_wait_result(&queued.ticket,&queued.owner.lease,blocking_observation_deadline())==KU_TASK_DRIVER_WAIT_READY);
  CHECK(ku_task_control_status(&queued.owner.lease)==KU_TASK_CONTROL_CANCELLED);
  blocking_no_file("queued-cancelled.txt"); blocking_no_file("queue-full.txt");
  CHECK(fixture_io_calls==1);
  KuTaskValueV1 reused=blocking_start("reused.txt"); blocking_take(&reused,NULL);
  KuResult_null untouched={0}; CHECK(ku_task_blocking_read(&old_ticket,&stale,0,&untouched)==KU_TASK_DRIVER_STALE);
  CHECK(!untouched.ok && !untouched.error.message.ptr);
  blocking_drop(&first); blocking_drop(&queued); blocking_drop(&reused); blocking_idle();
  KuTaskBlockingSnapshotV1 final=blocking_pool_snapshot();
  CHECK(!final.resident && !final.queued && !final.running && final.accepted==3 && final.finished==3);
  CHECK(final.rejected==1 && final.rejected_queue==1 && !final.fault);
  CHECK(!blocking_driver_snapshot().resident && !blocking_driver_snapshot().reserved_bytes);
  CHECK(remove("first.txt")==0); CHECK(remove("reused.txt")==0);
  blocking_finish(blocking_observation_deadline());
}
static void blocking_late_retirement_keeps_original_deadline(void) {
  blocking_setup(1,2u*sizeof(KuTaskInstance_0)+1024u); fixture_gate_io=1;
  KuTaskValueV1 task=blocking_start_owned_late();
  CHECK(ku_test_event_wait(&fixture_io_entered,2000)); blocking_idle();
  KuTaskInstance_0* instance=(KuTaskInstance_0*)task.owner.lease.control->context;
  size_t copies=instance->blocking.charged_bytes; CHECK(copies);
  CHECK(blocking_driver_snapshot().reserved_bytes==sizeof(KuTaskInstance_0)+31u+37u+copies);
  CHECK(!fixture_original_path_freed && !fixture_original_content_freed);
  KuTaskControlLeaseV1 late={0}; CHECK(ku_task_control_lease_retain(&task.owner.lease,&late)==KU_TASK_CONTROL_OK);
  uint64_t deadline=ku_task_driver_now_ms()+30u;
  KuTaskDriverCleanupReceiptV1 receipt={0};
  CHECK(ku_task_driver_owner_drop_receipt(&task.ticket,&task.owner,deadline,&receipt)==KU_TASK_DRIVER_OK);
  task=(KuTaskValueV1){0}; blocking_idle();
  // Cancellation has destroyed both original Owned View inputs while the
  // worker is still using its independent native path and content copies.
  CHECK(fixture_original_path_freed==1u && fixture_original_content_freed==1u);
  CHECK(blocking_driver_snapshot().reserved_bytes==sizeof(KuTaskInstance_0)+copies);
  CHECK(fixture_ledger().bytes==sizeof(KuTaskInstance_0)+copies);
  CHECK(ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_PENDING);
  CHECK(blocking_driver_snapshot().reserved_bytes>sizeof(KuTaskInstance_0));
  CHECK(ku_task_driver_shutdown(&blocking_driver,deadline)==KU_TASK_DRIVER_SHUTDOWN_TIMEOUT);
  CHECK(ku_task_blocking_finish(&blocking_pool,deadline)==KU_TASK_DRIVER_SHUTDOWN_TIMEOUT);
  CHECK(ku_task_driver_destroy(&blocking_driver)==KU_TASK_DRIVER_PENDING);
  KuTaskBlockingSnapshotV1 held=blocking_pool_snapshot(); CHECK(held.running==1 && held.resident==1 && !held.finished);
  CHECK(ku_task_control_cleanup_deadline(late.control)==deadline);
  CHECK(ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_PENDING);
  CHECK(ku_test_event_set(&fixture_io_release));
  // Driver idle describes executor work, not completion of the external job.
  // Observe its real condition/wake checkpoint before asking for executor idle.
  CHECK(!ku_task_blocking_lock(&blocking_pool));
  uint64_t completed_observer=blocking_observation_deadline();
  while (blocking_pool.finished!=1u)
    CHECK(ku_task_blocking_wait(&blocking_pool,completed_observer)==0);
  ku_task_blocking_unlock(&blocking_pool);
  blocking_idle();
  CHECK(ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_CLEANUP_ACK);
  CHECK(ku_task_control_status(&late)==KU_TASK_CONTROL_CANCELLED && ku_task_control_cleanup_deadline(late.control)==deadline);
  CHECK(blocking_driver_snapshot().reserved_bytes==sizeof(KuTaskInstance_0));
  CHECK(ku_task_control_lease_release(&late)==KU_TASK_CONTROL_OK); blocking_idle();
  // This is observation only; it never updates the original cleanup deadline.
  CHECK(!ku_task_blocking_lock(&blocking_pool));
  uint64_t observer=blocking_observation_deadline();
  while (blocking_pool.workers_exited!=blocking_pool.workers_created)
    CHECK(ku_task_blocking_wait(&blocking_pool,observer)==0);
  ku_task_blocking_unlock(&blocking_pool);
  CHECK(!blocking_driver_snapshot().resident && !blocking_driver_snapshot().reserved_bytes);
  FILE* file=fopen("late.txt","rb"); CHECK(file);
  uint8_t bytes[18]={0}; CHECK(fread(bytes,1,sizeof(bytes),file)==17u && !memcmp(bytes,"owned copy 世界",17u));
  CHECK(fclose(file)==0);
  CHECK(fixture_original_path_freed==1u && fixture_original_content_freed==1u);
  CHECK(remove("late.txt")==0); blocking_finish(deadline);
}
static void blocking_close_skips_queued_files(void) {
  blocking_setup(2,3u*sizeof(KuTaskInstance_0)+1024u); fixture_gate_io=1;
  KuTaskValueV1 running=blocking_start("closing-running.txt");
  CHECK(ku_test_event_wait(&fixture_io_entered,2000));
  KuTaskValueV1 queued=blocking_start("closing-queued.txt"); blocking_idle();
  CHECK(blocking_pool_snapshot().queued==1u);
  CHECK(ku_task_blocking_close(&blocking_pool)==KU_TASK_DRIVER_OK);
  CHECK(ku_test_event_set(&fixture_io_release)); blocking_take(&running,NULL);
  blocking_take(&queued,"blocking_pool_stopped");
  CHECK(fixture_io_calls==1u); blocking_no_file("closing-queued.txt");
  blocking_drop(&running); blocking_drop(&queued); blocking_idle();
  KuTaskBlockingSnapshotV1 snapshot=blocking_pool_snapshot();
  CHECK(!snapshot.resident && !snapshot.running && !snapshot.queued && snapshot.accepted==2u && snapshot.finished==2u);
  CHECK(remove("closing-running.txt")==0); blocking_finish(blocking_observation_deadline());
}
static void blocking_fault_is_reported_after_real_resource_cleanup(void) {
  blocking_setup(1,sizeof(KuTaskInstance_0)+1024u);
  fixture_fail_pool_signal=1;
  CHECK(ku_task_blocking_close(&blocking_pool)==KU_TASK_DRIVER_OK);
  CHECK(blocking_pool_snapshot().fault==KU_TASK_DRIVER_INTERNAL);
  uint64_t deadline=blocking_observation_deadline();
  CHECK(ku_task_driver_shutdown(&blocking_driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_blocking_finish(&blocking_pool,deadline)==KU_TASK_DRIVER_INTERNAL);
  CHECK(blocking_pool.initialized==KU_TASK_DRIVER_STORAGE_ZERO && !blocking_pool.resources);
  CHECK(blocking_pool.workers_created==blocking_pool.workers_joined);
  CHECK(ku_task_blocking_finish(&blocking_pool,deadline)==KU_TASK_DRIVER_INTERNAL);
  CHECK(ku_task_driver_join(&blocking_driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_destroy(&blocking_driver)==KU_TASK_DRIVER_OK);
  CHECK(ku_test_event_destroy(&fixture_io_entered)); CHECK(ku_test_event_destroy(&fixture_io_release));
  CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes && !fixture_ledger().overflow);
}
static void blocking_read_file(const char* name,size_t bytes) {
  FILE* file=fopen(name,"wb"); CHECK(file);
  uint8_t buffer[4096]; memset(buffer,'a',sizeof(buffer));
  while (bytes) { size_t part=bytes>sizeof(buffer) ? sizeof(buffer) : bytes; CHECK(fwrite(buffer,1,part,file)==part); bytes-=part; }
  CHECK(fclose(file)==0);
}
static KuTaskValueV1 blocking_read_start(const char* name) {
  KuString path=ku_string_static((const uint8_t*)name,strlen(name));
  KuTaskValueV1 task={0};
  CHECK(ku_task_2_start_value(&blocking_driver,&path,&task)==KU_TASK_DRIVER_OK);
  CHECK(task.tag==KU_TASK_VALUE_LIVE); return task;
}
static void blocking_read_take(KuTaskValueV1* task,size_t bytes,const char* error_code) {
  CHECK(ku_task_driver_wait_result(&task->ticket,&task->owner.lease,blocking_observation_deadline())==KU_TASK_DRIVER_WAIT_READY);
  KuTaskAdapterOutcomeV1 output={0}; CHECK(ku_task_value_take(task,NULL,&output)==KU_TASK_CONTROL_OK);
  CHECK(output.result_kind==4u && output.value.string.ok==(error_code==NULL));
  if (error_code) {
    KuString code=output.value.string.error.code;
    CHECK(code.len==strlen(error_code) && !memcmp(code.ptr,error_code,code.len));
  } else {
    KuString value=output.value.string.value;
    CHECK(value.len==bytes && value.capacity>=bytes && value.storage==KU_STRING_OWNED);
    for (size_t i=0;i<bytes;i++) CHECK(value.ptr[i]=='a');
    CHECK(blocking_driver_snapshot().reserved_bytes==sizeof(KuTaskInstance_2));
  }
  ku_task_outcome_drop(&output);
}
static void blocking_read_boundaries_and_failures(void) {
  // Explicit test-only 3 MB dynamic allowance covers real old+new growth peaks;
  // the executable root budget formula is unchanged.
  blocking_setup(2,3u*1000000u+4u*sizeof(KuTaskInstance_2));
  for (unsigned mode=0;mode<4;mode++) {
    size_t length=mode==0 ? 0u : mode==1 ? 45000u : mode==2 ? 1000000u : 1000001u;
    blocking_read_file("read-input.txt",length);
    KuTaskValueV1 task=blocking_read_start("read-input.txt");
    blocking_read_take(&task,length,mode==3 ? "file_too_large" : NULL);
    blocking_drop(&task); blocking_idle();
    CHECK(!blocking_driver_snapshot().reserved_bytes && !fixture_ledger().bytes);
  }
  CHECK(fixture_output_calls>4u);
  // Driver accounting must track live old+new capacities, not their historical sum.
  CHECK(blocking_driver_snapshot().peak_reserved_bytes<1600000u);
  CHECK(remove("read-input.txt")==0); blocking_finish(blocking_observation_deadline());
  for (size_t fail=0;fail<5;fail++) {
    blocking_setup(1,3u*1000000u+2u*sizeof(KuTaskInstance_2));
    blocking_read_file("read-fail.txt",45000u); fixture_fail_output_in=fail;
    KuTaskValueV1 task=blocking_read_start("read-fail.txt"); blocking_read_take(&task,0,"out_of_memory");
    blocking_drop(&task); blocking_idle(); CHECK(!fixture_ledger().bytes && !blocking_driver_snapshot().reserved_bytes);
    CHECK(remove("read-fail.txt")==0); blocking_finish(blocking_observation_deadline());
  }
  blocking_setup(1,sizeof(KuTaskInstance_2)+1024u);
  blocking_read_file("read-limit.txt",1u);
  KuTaskValueV1 limited=blocking_read_start("read-limit.txt"); blocking_read_take(&limited,0,"memory_limit");
  CHECK(!fixture_output_calls); blocking_drop(&limited); blocking_idle();
  CHECK(remove("read-limit.txt")==0); blocking_finish(blocking_observation_deadline());
}
static void blocking_read_cancel_holds_real_output_until_done(void) {
  blocking_setup(1,100000u+2u*sizeof(KuTaskInstance_2));
  blocking_read_file("read-cancel.txt",45000u); fixture_gate_output=1;
  KuTaskValueV1 task=blocking_read_start("read-cancel.txt");
  CHECK(ku_test_event_wait(&fixture_io_entered,2000)); blocking_idle();
  KuTaskInstance_2* instance=(KuTaskInstance_2*)task.owner.lease.control->context;
  KuTaskBlockingTokenV1 token=instance->blocking;
  CHECK(!ku_task_blocking_lock(&blocking_pool));
  CHECK(blocking_pool.jobs[token.slot].output_charge==4096u);
  ku_task_blocking_unlock(&blocking_pool);
  KuTaskControlLeaseV1 late={0}; CHECK(ku_task_control_lease_retain(&task.owner.lease,&late)==KU_TASK_CONTROL_OK);
  uint64_t deadline=ku_task_driver_now_ms()+30u;
  KuTaskDriverCleanupReceiptV1 receipt={0};
  CHECK(ku_task_driver_owner_drop_receipt(&task.ticket,&task.owner,deadline,&receipt)==KU_TASK_DRIVER_OK);
  task=(KuTaskValueV1){0}; blocking_idle();
  CHECK(ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_PENDING);
  CHECK(blocking_driver_snapshot().reserved_bytes==sizeof(KuTaskInstance_2)+token.charged_bytes+4096u);
  CHECK(ku_task_driver_shutdown(&blocking_driver,deadline)==KU_TASK_DRIVER_SHUTDOWN_TIMEOUT);
  CHECK(ku_task_blocking_finish(&blocking_pool,deadline)==KU_TASK_DRIVER_SHUTDOWN_TIMEOUT);
  CHECK(ku_task_control_cleanup_deadline(late.control)==deadline);
  CHECK(ku_test_event_set(&fixture_io_release));
  CHECK(!ku_task_blocking_lock(&blocking_pool));
  uint64_t observer=blocking_observation_deadline();
  while (blocking_pool.finished!=1u) CHECK(ku_task_blocking_wait(&blocking_pool,observer)==0);
  ku_task_blocking_unlock(&blocking_pool); blocking_idle();
  CHECK(ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_CLEANUP_ACK);
  CHECK(fixture_output_calls==1u); // Cancelled growth is rejected before malloc.
  CHECK(blocking_driver_snapshot().reserved_bytes==sizeof(KuTaskInstance_2));
  CHECK(ku_task_control_cleanup_deadline(late.control)==deadline);
  CHECK(ku_task_control_lease_release(&late)==KU_TASK_CONTROL_OK); blocking_idle();
  CHECK(!ku_task_blocking_lock(&blocking_pool)); observer=blocking_observation_deadline();
  while (blocking_pool.workers_exited!=blocking_pool.workers_created) CHECK(ku_task_blocking_wait(&blocking_pool,observer)==0);
  ku_task_blocking_unlock(&blocking_pool);
  CHECK(remove("read-cancel.txt")==0); blocking_finish(deadline);
}
static void blocking_read_done_cancellation_drops_owned_output(void) {
  blocking_setup(1,100000u+2u*sizeof(KuTaskInstance_2));
  blocking_read_file("read-done.txt",1u); fixture_gate_output=1;
  KuTaskValueV1 task=blocking_read_start("read-done.txt");
  CHECK(ku_test_event_wait(&fixture_io_entered,2000)); blocking_idle();
  CHECK(ku_task_control_lease_retain(&task.owner.lease,&fixture_read_done_lease)==KU_TASK_CONTROL_OK);
  fixture_read_done_deadline=blocking_observation_deadline();
  CHECK(ku_test_event_set(&fixture_io_release));
  CHECK(ku_task_driver_wait_result(&task.ticket,&task.owner.lease,blocking_observation_deadline())==KU_TASK_DRIVER_WAIT_READY);
  CHECK(ku_task_control_status(&task.owner.lease)==KU_TASK_CONTROL_CANCELLED);
  blocking_drop(&task); blocking_idle();
  CHECK(!blocking_pool_snapshot().resident);
  CHECK(blocking_driver_snapshot().reserved_bytes==sizeof(KuTaskInstance_2));
  CHECK(fixture_ledger().bytes==sizeof(KuTaskInstance_2));
  CHECK(ku_task_control_lease_release(&fixture_read_done_lease)==KU_TASK_CONTROL_OK); blocking_idle();
  CHECK(remove("read-done.txt")==0); blocking_finish(blocking_observation_deadline());
}
static void blocking_rejections_have_no_file_effect(void) {
  for (unsigned mode=0;mode<4;mode++) {
    blocking_setup(1,sizeof(KuTaskInstance_0)+(mode ? 1024u : 0u));
    if (mode==1 || mode==2) fixture_fail_blocking_in=mode-1u;
    if (mode==3) CHECK(ku_task_blocking_close(&blocking_pool)==KU_TASK_DRIVER_OK);
    KuTaskValueV1 task=blocking_start("rejected.txt");
    blocking_take(&task,mode==0 ? "memory_limit" : mode==3 ? "blocking_pool_stopped" : "out_of_memory");
    CHECK(blocking_driver_snapshot().reserved_bytes==sizeof(KuTaskInstance_0));
    CHECK(fixture_ledger().allocations==1 && !fixture_io_calls);
    blocking_no_file("rejected.txt"); blocking_drop(&task); blocking_idle();
    KuTaskBlockingSnapshotV1 final=blocking_pool_snapshot();
    CHECK(!final.resident && !final.running && !final.queued && !final.accepted && !final.finished && final.rejected==1);
    blocking_finish(blocking_observation_deadline());
  }
}
int main(void) {
  CHECK(KU_TASK_FRAME_ABI_VERSION==5u && KU_TASK_CONTROL_ABI_VERSION==2u && KU_TASK_DRIVER_ABI_VERSION==10u);
  blocking_queue_and_generation(); blocking_late_retirement_keeps_original_deadline();
  blocking_rejections_have_no_file_effect(); blocking_close_skips_queued_files();
  blocking_fault_is_reported_after_real_resource_cleanup();
  blocking_read_boundaries_and_failures(); blocking_read_cancel_holds_real_output_until_done();
  blocking_read_done_cancellation_drops_owned_output();
  puts("blocking-write-ledger-ok"); return 0;
}
"#;

#[test]
fn native_task_callable_parameters_validate_all_ids_before_reserving_or_moving() {
    let source = r#"
async fn First(value: int): int! { return ok(value + 1) }
async fn Second(value: int): int! { return ok(value + 2) }
async fn Unused(value: int): int! { return ok(value + 3) }
async fn Receiver(text: str, left: async fn(int): int!, right: async fn(int): int!): str! {
    first = (await left(1))?
    second = (await right(2))?
    return ok(text)
}
async fn Dormant(selected: async fn(int): int!, value: int): int! {
    return await selected(value)
}
async fn main(): null! {
    text = "source"
    ignored = await Receiver(text, Second, First)
    return ok(null)
}
"#;
    let generated = finally_adapter_generated(source, "");
    let anchor = "/* Internal typed adapter ABI; not source Task syntax or a stable C FFI.";
    assert_eq!(generated.matches(anchor).count(), 1);
    let mut generated =
        generated.replacen(anchor, &format!("{CALLABLE_PARAMETER_HOOKS}\n{anchor}"), 1);
    generated.push_str(CALLABLE_PARAMETER_MAIN);
    run_finally_adapter_generated(
        generated,
        "native-task-callable-parameters",
        "callable-parameters-ok\n",
    );
}

const CALLABLE_PARAMETER_HOOKS: &str = r#"
static int fixture_parameter_fail_commit;
static KuString* fixture_parameter_input;
static uint32_t *fixture_parameter_left,*fixture_parameter_right;
static uint32_t fixture_parameter_commit(const KuTaskDriverTicketV1* ticket,
    KuTaskControlOwnerV1* owner,uint32_t mode,uint64_t deadline) {
  if (fixture_parameter_fail_commit) {
    CHECK(mode==KU_TASK_DRIVER_START && !fixture_parameter_input->ptr);
    CHECK(!*fixture_parameter_left && !*fixture_parameter_right);
    return KU_TASK_DRIVER_INTERNAL;
  }
  return ku_task_driver_commit(ticket,owner,mode,deadline);
}
#define ku_task_driver_commit fixture_parameter_commit
"#;

const CALLABLE_PARAMETER_MAIN: &str = r#"
#undef ku_task_driver_commit
typedef struct FixtureParameterRuntime {
  KuTaskDriverV1 driver;
  KuTaskDriverSlotV1 slots[4];
  size_t ring[4];
} FixtureParameterRuntime;
static uint64_t fixture_parameter_deadline(void) { return ku_task_driver_now_ms()+2000u; }
static void fixture_parameter_init(FixtureParameterRuntime* runtime,size_t budget) {
  memset(runtime,0,sizeof(*runtime));
  size_t fixed=sizeof(runtime->driver)+sizeof(runtime->slots)+sizeof(runtime->ring);
  CHECK(ku_task_driver_init(&runtime->driver,sizeof(runtime->driver),KU_TASK_DRIVER_ABI_VERSION,
    runtime->slots,4u,runtime->ring,4u,fixed+budget,1u,fixture_parameter_deadline())==KU_TASK_DRIVER_OK);
}
static void fixture_parameter_empty(FixtureParameterRuntime* runtime) {
  CHECK(ku_task_driver_wait_idle(&runtime->driver,fixture_parameter_deadline())==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot={0};
  CHECK(ku_task_driver_snapshot(&runtime->driver,&snapshot)==KU_TASK_DRIVER_OK);
  CHECK(!snapshot.fault && !snapshot.resident && !snapshot.reserved_bytes && !snapshot.building
    && !snapshot.running && !snapshot.queued);
}
static void fixture_parameter_unreserved(FixtureParameterRuntime* runtime) {
  fixture_parameter_empty(runtime);
  KuTaskDriverSnapshotV1 snapshot={0};
  CHECK(ku_task_driver_snapshot(&runtime->driver,&snapshot)==KU_TASK_DRIVER_OK);
  /* A reserve followed by rollback must still leave a nonzero peak. */
  CHECK(!snapshot.peak_resident && !snapshot.peak_reserved_bytes
    && !snapshot.peak_queued && !snapshot.peak_running);
}
static void fixture_parameter_finish(FixtureParameterRuntime* runtime) {
  fixture_parameter_empty(runtime);
  CHECK(ku_task_driver_shutdown(&runtime->driver,fixture_parameter_deadline())==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_join(&runtime->driver,runtime->driver.shutdown_deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_destroy(&runtime->driver)==KU_TASK_DRIVER_OK);
}
static KuString fixture_parameter_owned(void) {
  uint8_t* bytes=(uint8_t*)malloc(64u); CHECK(bytes); memcpy(bytes,"owned!",6u);
  return (KuString){bytes,6u,64u,KU_STRING_OWNED};
}
static int fixture_parameter_same(KuString a,KuString b) {
  return a.ptr==b.ptr && a.len==b.len && a.capacity==b.capacity && a.storage==b.storage;
}
static void fixture_parameter_rejections(void) {
  FixtureParameterRuntime runtime;
  fixture_parameter_init(&runtime,sizeof(KuTaskInstance_3)+2u*sizeof(KuTaskInstance_0)+1024u);
  KuString text=fixture_parameter_owned(),original=text;
  uint32_t left=1u,right=2u; /* Same signature, but right=Unused was never proved. */
  KuTaskHandle_3 handle={0}; KuTaskValueV1 value={0};
  KuTaskFrame_3 frame={0};
  FixtureLedger before=fixture_ledger();
  CHECK(ku_task_frame_3_init(&frame,sizeof(frame),KU_TASK_FRAME_ABI_VERSION,&text,&left,&right)==KU_TASK_FRAME_INVALID_ARGUMENT);
  CHECK(ku_task_frame_zero_bytes(&frame,sizeof(frame)));
  CHECK(ku_task_3_try_start(&runtime.driver,&text,&left,&right,&handle)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(ku_task_3_start_value(&runtime.driver,&text,&left,&right,&value)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  KuTaskDriverTicketV1 parent={0}; parent.driver=&runtime.driver;
  KuTaskControlV1 control={0};
  KuTaskAdapterHostV1 host={0}; host.ticket=&parent; host.control=&control; host.minimum_charge=1u;
  CHECK(ku_task_3_start_hosted(&host,&text,&left,&right,&value)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(left==1u && right==2u && fixture_parameter_same(text,original));
  CHECK(ku_task_frame_zero_bytes(&handle,sizeof(handle)) && ku_task_frame_zero_bytes(&value,sizeof(value)));
  CHECK(fixture_ledger().calls==before.calls);
  fixture_parameter_unreserved(&runtime);
  uint32_t dormant=0u; int64_t input=7;
  KuTaskHandle_4 absent={0}; KuTaskFrame_4 absent_frame={0};
  CHECK(ku_task_frame_4_init(&absent_frame,sizeof(absent_frame),KU_TASK_FRAME_ABI_VERSION,&dormant,&input)==KU_TASK_FRAME_INVALID_ARGUMENT);
  CHECK(ku_task_4_try_start(&runtime.driver,&dormant,&input,&absent)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(dormant==0u && input==7 && ku_task_frame_zero_bytes(&absent_frame,sizeof(absent_frame)));
  CHECK(fixture_ledger().calls==before.calls);
  fixture_parameter_unreserved(&runtime);
  ku_string_drop(&text);

  /* An actual one-byte allocation cannot contain a uint32_t identity. The
   * adapter must reject the deep alias before it tries reading that identity. */
  KuString tiny=ku_string_concat((KuString){0},(KuString){0}),tiny_original=tiny;
  CHECK(tiny.ptr && !tiny.len && !tiny.capacity && tiny.storage==KU_STRING_OWNED);
  before=fixture_ledger(); left=1u;
  uint32_t* alias=(uint32_t*)(void*)tiny.ptr;
  CHECK(ku_task_3_try_start(&runtime.driver,&tiny,&left,alias,&handle)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(ku_task_3_start_value(&runtime.driver,&tiny,&left,alias,&value)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(ku_task_3_start_hosted(&host,&tiny,&left,alias,&value)==KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(left==1u && fixture_parameter_same(tiny,tiny_original));
  CHECK(fixture_ledger().calls==before.calls);
  fixture_parameter_unreserved(&runtime);
  ku_string_drop(&tiny);
  fixture_parameter_finish(&runtime);
}
static void fixture_parameter_rollback_and_success(void) {
  FixtureParameterRuntime runtime;
  fixture_parameter_init(&runtime,sizeof(KuTaskInstance_3)+2u*sizeof(KuTaskInstance_0)+1024u);
  KuString text=fixture_parameter_owned(),original=text;
  uint32_t left=1u,right=0u; /* First really is function ID zero. */
  KuTaskHandle_3 handle={0};
  FixtureLedger before=fixture_ledger();
  fixture_parameter_input=&text; fixture_parameter_left=&left; fixture_parameter_right=&right;
  fixture_parameter_fail_commit=1;
  CHECK(ku_task_3_try_start(&runtime.driver,&text,&left,&right,&handle)==KU_TASK_DRIVER_INTERNAL);
  fixture_parameter_fail_commit=0;
  CHECK(left==1u && right==0u && fixture_parameter_same(text,original));
  CHECK(!memcmp(text.ptr,"owned!",6u));
  CHECK(ku_task_frame_zero_bytes(&handle,sizeof(handle)));
  CHECK(fixture_ledger().calls==before.calls+1u
    && fixture_ledger().bytes==before.bytes && fixture_ledger().allocations==before.allocations);
  fixture_parameter_empty(&runtime);
  before=fixture_ledger();
  CHECK(ku_task_3_try_start(&runtime.driver,&text,&left,&right,&handle)==KU_TASK_DRIVER_OK);
  CHECK(!text.ptr && left==0u && right==0u);
  CHECK(ku_task_driver_wait_idle(&runtime.driver,fixture_parameter_deadline())==KU_TASK_DRIVER_OK);
  /* Receiver plus its two actual children; copying IDs allocates nothing. */
  CHECK(fixture_ledger().calls==before.calls+3u);
  KuTaskAdapterOutcomeV1 output={0};
  CHECK(ku_task_3_take(&handle,&output)==KU_TASK_CONTROL_OK && output.value.string.ok);
  CHECK(fixture_parameter_same(output.value.string.value,original));
  CHECK(!memcmp(output.value.string.value.ptr,"owned!",6u));
  CHECK(ku_task_3_drop(&handle,fixture_parameter_deadline())==KU_TASK_DRIVER_OK);
  ku_task_outcome_drop(&output);
  fixture_parameter_finish(&runtime);
}
static void fixture_parameter_inline_rejection(void) {
  for (unsigned closed=0;closed<2u;++closed) {
    FixtureParameterRuntime runtime; fixture_parameter_init(&runtime,closed ? 1024u*1024u : 0u);
    if (closed) CHECK(ku_task_driver_shutdown(&runtime.driver,fixture_parameter_deadline())==KU_TASK_DRIVER_OK);
    KuString text=fixture_parameter_owned(); uint32_t left=1u,right=0u;
    KuTaskValueV1 rejected={0};
    CHECK(ku_task_3_start_value(&runtime.driver,&text,&left,&right,&rejected)==KU_TASK_DRIVER_OK);
    CHECK(rejected.tag==KU_TASK_VALUE_INLINE_FAILED
      && rejected.rejection_code==(closed ? KU_TASK_DRIVER_CLOSED : KU_TASK_DRIVER_LIMIT));
    CHECK(!text.ptr && !left && !right);
    CHECK(ku_task_value_drop(&rejected,fixture_parameter_deadline())==KU_TASK_DRIVER_OK);
    fixture_parameter_finish(&runtime);
  }
}
int main(void) {
  fixture_parameter_rejections(); fixture_parameter_rollback_and_success(); fixture_parameter_inline_rejection();
  FixtureLedger final=fixture_ledger();
  CHECK(!final.allocations && !final.bytes && !final.overflow);
  puts("callable-parameters-ok"); return 0;
}
"#;

#[test]
fn native_task_flat_array_snapshots_charge_partial_owners_and_release_each_failure() {
    let source = r#"
async fn Inspect(left: str, right: str, stop: bool): int! {
    values=[left,right]
    count=0
    for text in values {
        count=count+1
        if (stop) { break }
        if (count==1) { continue }
    }
    if (values.len()!=2) { fail "source lost" }
    return ok(count)
}
async fn main(): null! { return ok(null) }
"#;
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let native = ir::task_lower::lower_program(&ast).unwrap();
    let mut source_slot = None;
    let mut snapshot_slot = None;
    for operation in native.tasks.functions[0]
        .states
        .iter()
        .flat_map(|state| &state.operations)
    {
        if let TaskOp::ArrayClone { dst, src } = operation {
            source_slot = Some(src.0);
            snapshot_slot = Some(dst.0);
        }
    }
    let hooks = r#"
static KuTestEvent array_entered,array_proceed;
static size_t array_calls,array_fail_at,array_gate_at;
static void* array_malloc(size_t bytes) {
  fixture_alloc_lock(); size_t call=++array_calls;
  int fail=call==array_fail_at,gate=call==array_gate_at; fixture_alloc_unlock();
  if (gate) { CHECK(ku_test_event_set(&array_entered)); CHECK(ku_test_event_wait(&array_proceed,2000)); }
  return fail ? NULL : fixture_malloc(bytes);
}
"#;
    let mut generated = finally_adapter_generated(source, hooks)
        .replacen(
            "void* data=malloc(bytes);",
            "void* data=array_malloc(bytes);",
            1,
        )
        .replacen(
            "uint8_t* bytes=(uint8_t*)malloc(capacity);",
            "uint8_t* bytes=(uint8_t*)array_malloc(capacity);",
            1,
        );
    generated.push_str(
        &ARRAY_SNAPSHOT_MAIN
            .replace("@SOURCE@", &source_slot.unwrap().to_string())
            .replace("@SNAPSHOT@", &snapshot_slot.unwrap().to_string()),
    );
    run_finally_adapter_generated(
        generated,
        "native-task-flat-array-owners",
        "array-snapshot-owners-ok\n",
    );
}

const ARRAY_SNAPSHOT_MAIN: &str = r#"
typedef struct ArrayRuntime { KuTaskDriverV1 driver; KuTaskDriverSlotV1 slots[2]; size_t ring[2]; } ArrayRuntime;
static uint64_t array_deadline(void) { return ku_task_driver_now_ms()+2000u; }
static KuTaskDriverSnapshotV1 array_snapshot(ArrayRuntime* runtime) {
  KuTaskDriverSnapshotV1 snapshot={0}; CHECK(ku_task_driver_snapshot(&runtime->driver,&snapshot)==KU_TASK_DRIVER_OK); return snapshot;
}
static void array_init(ArrayRuntime* runtime,size_t budget) {
  memset(runtime,0,sizeof(*runtime)); array_calls=array_fail_at=array_gate_at=0;
  CHECK(ku_test_event_init(&array_entered)); CHECK(ku_test_event_init(&array_proceed));
  size_t fixed=sizeof(runtime->driver)+sizeof(runtime->slots)+sizeof(runtime->ring);
  CHECK(ku_task_driver_init(&runtime->driver,sizeof(runtime->driver),KU_TASK_DRIVER_ABI_VERSION,
      runtime->slots,2u,runtime->ring,2u,fixed+budget,1u,array_deadline())==KU_TASK_DRIVER_OK);
}
static void array_idle(ArrayRuntime* runtime) { CHECK(ku_task_driver_wait_idle(&runtime->driver,array_deadline())==KU_TASK_DRIVER_OK); }
static KuTaskValueV1 array_start(ArrayRuntime* runtime,bool stop) {
  uint8_t* first=(uint8_t*)fixture_malloc(31u); uint8_t* second=(uint8_t*)fixture_malloc(37u); CHECK(first&&second);
  memcpy(first,"first",5u); memcpy(second,"second",6u);
  KuString left={first,5u,31u,KU_STRING_OWNED},right={second,6u,37u,KU_STRING_OWNED}; KuTaskValueV1 task={0};
  CHECK(ku_task_0_start_value(&runtime->driver,&left,&right,&stop,&task)==KU_TASK_DRIVER_OK);
  CHECK(task.tag==KU_TASK_VALUE_LIVE&&!left.ptr&&!right.ptr); return task;
}
static void array_finish(ArrayRuntime* runtime) {
  array_idle(runtime); CHECK(!array_snapshot(runtime).resident&&!array_snapshot(runtime).reserved_bytes);
  uint64_t deadline=array_deadline(); CHECK(ku_task_driver_shutdown(&runtime->driver,deadline)==KU_TASK_DRIVER_OK);
  uint64_t effective=runtime->driver.shutdown_deadline;
  KuTaskDriverSnapshotV1 snapshot=array_snapshot(runtime);
  CHECK(snapshot.workers_created==1u && snapshot.workers_exited==1u && !snapshot.workers_joined
      && !snapshot.workers_waiting && !snapshot.resident && !snapshot.reserved_bytes
      && !snapshot.building && !snapshot.queued && !snapshot.running);
#if defined(_WIN32)
  // As in fixture_reap, observe the OS-return tail separately after logical
  // EXITED and all Task storage is gone. This never renews the original D.
  if (ku_task_driver_now_ms()>=effective)
    CHECK(WaitForSingleObject(runtime->driver.workers[0].thread,2000)==WAIT_OBJECT_0);
#else
  alarm(2); // Existing independent observation bound for the POSIX join tail.
#endif
  CHECK(ku_task_driver_join(&runtime->driver,effective)==KU_TASK_DRIVER_OK);
  CHECK(runtime->driver.shutdown_deadline==effective && runtime->driver.workers_joined==1u
      && runtime->driver.workers[0].joined && runtime->driver.workers[0].closed);
  CHECK(ku_task_driver_destroy(&runtime->driver)==KU_TASK_DRIVER_OK);
#if !defined(_WIN32)
  alarm(0);
#endif
  CHECK(ku_test_event_destroy(&array_entered)); CHECK(ku_test_event_destroy(&array_proceed));
  CHECK(!fixture_ledger().allocations&&!fixture_ledger().bytes&&!fixture_ledger().overflow);
}
static void array_take(ArrayRuntime* runtime,KuTaskValueV1* task,int64_t expected,const char* error) {
  CHECK(ku_task_driver_wait_result(&task->ticket,&task->owner.lease,array_deadline())==KU_TASK_DRIVER_WAIT_READY);
  KuTaskAdapterOutcomeV1 output={0}; CHECK(ku_task_value_take(task,NULL,&output)==KU_TASK_CONTROL_OK);
  CHECK(output.result_kind==1u&&output.value.integer.ok==(error==NULL));
  if (error) {
    KuString code=output.value.integer.error.code; CHECK(code.len==strlen(error)&&!memcmp(code.ptr,error,code.len));
    CHECK(output.exit_class==KU_TASK_EXIT_RUNTIME_FAILURE);
  } else CHECK(output.value.integer.value==expected);
  CHECK(array_snapshot(runtime).reserved_bytes==sizeof(KuTaskInstance_0));
  CHECK(fixture_ledger().bytes==sizeof(KuTaskInstance_0)); ku_task_outcome_drop(&output);
  CHECK(ku_task_value_drop(task,array_deadline())==KU_TASK_DRIVER_OK);
}
int main(void) {
  CHECK(KU_TASK_FRAME_ABI_VERSION==5u&&KU_TASK_CONTROL_ABI_VERSION==2u&&KU_TASK_DRIVER_ABI_VERSION==10u);
  for (unsigned mode=0;mode<7;mode++) {
    ArrayRuntime runtime;
    size_t budget=sizeof(KuTaskInstance_0)+68u+(mode==5 ? 2u*sizeof(KuString) : 4096u);
    array_init(&runtime,budget);
    if (mode>=1&&mode<=4) array_fail_at=mode;
    if (mode==4) array_gate_at=4u;
    KuTaskValueV1 task=array_start(&runtime,mode==6);
    if (mode==4) {
      CHECK(ku_test_event_wait(&array_entered,2000));
      KuTaskInstance_0* instance=(KuTaskInstance_0*)task.owner.lease.control->context;
      CHECK(instance->frame.header.initialized&(UINT64_C(1)<<@SNAPSHOT@));
      CHECK(instance->frame.s_@SOURCE@.len==2u&&instance->frame.s_@SNAPSHOT@.len==1u);
      CHECK(instance->frame.s_@SNAPSHOT@.capacity==2u);
      CHECK(instance->frame.s_@SOURCE@.data[0].capacity==31u&&instance->frame.s_@SOURCE@.data[1].capacity==37u);
      size_t actual=sizeof(KuTaskInstance_0)+68u+4u*sizeof(KuString)+5u;
      CHECK(fixture_ledger().bytes==actual&&array_snapshot(&runtime).reserved_bytes==actual+6u);
      CHECK(ku_test_event_set(&array_proceed));
    }
    array_take(&runtime,&task,mode==6 ? 1 : 2,mode==5 ? "memory_limit" : mode ? (mode==6 ? NULL : "out_of_memory") : NULL);
    if (mode==5) CHECK(array_calls==1u); // Snapshot rejection precedes malloc.
    array_finish(&runtime);
  }
  // Cancellation after a partial snapshot retains physical owners until the
  // executing callback returns; the original cleanup deadline is not renewed.
  ArrayRuntime runtime; array_init(&runtime,sizeof(KuTaskInstance_0)+4096u); array_gate_at=4u;
  KuTaskValueV1 task=array_start(&runtime,false); CHECK(ku_test_event_wait(&array_entered,2000));
  KuTaskControlLeaseV1 late={0}; CHECK(ku_task_control_lease_retain(&task.owner.lease,&late)==KU_TASK_CONTROL_OK);
  uint64_t deadline=ku_task_driver_now_ms()+30u; KuTaskDriverCleanupReceiptV1 receipt={0};
  CHECK(ku_task_driver_owner_drop_receipt(&task.ticket,&task.owner,deadline,&receipt)==KU_TASK_DRIVER_OK); task=(KuTaskValueV1){0};
  CHECK(ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_PENDING);
  CHECK(ku_task_driver_shutdown(&runtime.driver,deadline)==KU_TASK_DRIVER_SHUTDOWN_TIMEOUT);
  CHECK(ku_task_control_cleanup_deadline(late.control)==deadline);
  CHECK(ku_test_event_set(&array_proceed)); array_idle(&runtime);
  CHECK(ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_CLEANUP_ACK);
  CHECK(ku_task_control_cleanup_deadline(late.control)==deadline);
  CHECK(array_snapshot(&runtime).reserved_bytes==sizeof(KuTaskInstance_0));
  CHECK(fixture_ledger().bytes==sizeof(KuTaskInstance_0));
  CHECK(ku_task_control_lease_release(&late)==KU_TASK_CONTROL_OK); array_finish(&runtime);
  puts("array-snapshot-owners-ok"); return 0;
}
"#;
