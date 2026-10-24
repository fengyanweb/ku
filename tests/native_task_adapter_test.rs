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
    assert!(adapter.contains("ku_task_driver_begin_quantum"));
    let default_intent = generated
        .split_once("static uint32_t ku_task_driver_begin_quantum(")
        .expect("generated default-intent helper")
        .1
        .split_once("\n}\n")
        .expect("default-intent helper boundary")
        .0;
    assert!(default_intent.contains("execution->default_yield=1;"));
    assert!(
        default_intent.contains("return ku_task_driver_set_intent(ticket,KU_TASK_DRIVER_YIELD);")
    );
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
        assert!(
            resume.contains("ku_task_driver_begin_quantum(&instance->ticket,&instance->control)")
        );
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
  CHECK(KU_TASK_FRAME_ABI_VERSION == 5u && KU_TASK_DRIVER_ABI_VERSION == 11u);
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
async fn Probe(path: str): bool! {
    present=fs.exists(path)
    return ok(present)
}
async fn List(path: str): [str]! { return fs.read_dir(path) }
async fn Carry(values: [str]): [str]! { return ok(values) }
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
    // The cancellation witness must stay attached to the real worker's DONE
    // publication, including when network jobs introduce a deferred state.
    assert_eq!(generated.matches("    pool->running--;").count(), 1);
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
        .replacen(
            "static bool ku_fs_exists_native_impl(const KuFsNativeChar* native) {",
            "static bool ku_fs_exists_native_impl(const KuFsNativeChar* native) {\n  fixture_io_gate();",
            1,
        )
        .replace(
            "static KuResult_array_str ku_fs_read_dir_native_impl(const KuFsNativeChar* native, KuFsAllocatorV1* allocator) {",
            "static KuResult_array_str ku_fs_read_dir_native_impl(const KuFsNativeChar* native, KuFsAllocatorV1* allocator) {\n  fixture_io_gate();",
        )
        .replacen("void* pointer=malloc(bytes);", "void* pointer=fixture_output_malloc(bytes);", 1)
        .replacen("    pool->running--;", "    pool->running--; fixture_blocking_done(job->kind);", 1)
        .replacen("pool->storage=calloc(1,pool->storage_bytes);", "pool->storage=fixture_pool_calloc(1,pool->storage_bytes);", 1)
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
    fs::create_dir(directory.path().join("listing")).unwrap();
    fs::create_dir(directory.path().join("empty")).unwrap();
    for index in 0..80 {
        fs::write(directory.path().join(format!("listing/f{index:03}")), b"").unwrap();
    }
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
    let layout = String::from_utf8_lossy(&output.stderr);
    assert!(layout.contains("file-job-layout job="), "{layout}");
    eprint!("{layout}");
}

const BLOCKING_FIXTURE_HOOKS: &str = r#"
static KuTestEvent fixture_io_entered,fixture_io_release;
static int fixture_gate_io;
static size_t fixture_io_calls,fixture_fail_blocking_in=SIZE_MAX;
static int fixture_fail_pool_storage;
static void* fixture_pool_calloc(size_t count,size_t bytes) {
  if(fixture_fail_pool_storage) { fixture_fail_pool_storage=0; return NULL; }
  return fixture_calloc(count,bytes);
}

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
/* Pool arrays are now heap-backed fixed storage, charged in fixed_bytes.
 * Keep the pre-existing per-work assertions while checking that fixed backing
 * separately and requiring the full allocation ledger to reach zero at finish. */
static FixtureLedger blocking_work_ledger(void) {
  FixtureLedger ledger=fixture_ledger();
  if(blocking_pool.storage) {
    CHECK(ledger.allocations && ledger.bytes>=blocking_pool.storage_bytes);
    ledger.allocations--;ledger.bytes-=blocking_pool.storage_bytes;
  }
  return ledger;
}

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
  size_t fixed=sizeof(blocking_driver)+sizeof(blocking_slots)+sizeof(blocking_ring)+ku_task_blocking_fixed_bytes(capacity);
  uint64_t deadline=blocking_observation_deadline();
  CHECK(ku_task_driver_init(&blocking_driver,sizeof(blocking_driver),KU_TASK_DRIVER_ABI_VERSION,
      blocking_slots,4,blocking_ring,4,fixed+dynamic_budget,1,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_blocking_init(&blocking_pool,capacity,1,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_attach_blocking(&blocking_driver,&blocking_pool,sizeof(blocking_pool),
      ku_task_blocking_close,ku_task_blocking_ready,blocking_pool.storage,blocking_pool.storage_bytes)==KU_TASK_DRIVER_OK);
  CHECK(blocking_driver_snapshot().fixed_bytes==fixed);
  CHECK(fixture_ledger().allocations==1u && fixture_ledger().bytes==blocking_pool.storage_bytes);
  CHECK(!ku_task_driver_external_storage(&blocking_driver,&blocking_pool.jobs[0],sizeof(blocking_pool.jobs[0]),1));
  KuString alias=ku_string_static((const uint8_t*)&blocking_pool.jobs[0],1);
  CHECK(ku_task_adapter_string_overlaps_runtime(alias,&blocking_driver));
}
static void blocking_finish(uint64_t deadline) {
  CHECK(ku_task_driver_shutdown(&blocking_driver,deadline)==KU_TASK_DRIVER_OK);
  uint32_t finished=ku_task_blocking_finish(&blocking_pool,deadline);
  if (finished!=KU_TASK_DRIVER_OK) fprintf(stderr,
      "blocking finish status=%u deadline=%llu now=%llu exited=%zu created=%zu joined=%zu\n",
      finished,(unsigned long long)deadline,(unsigned long long)ku_task_driver_now_ms(),
      blocking_pool.workers_exited,blocking_pool.workers_created,blocking_pool.workers_joined);
  CHECK(finished==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_join(&blocking_driver,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_destroy(&blocking_driver)==KU_TASK_DRIVER_OK);
  CHECK(ku_test_event_destroy(&fixture_io_entered)); CHECK(ku_test_event_destroy(&fixture_io_release));
  FixtureLedger ledger=blocking_work_ledger(); CHECK(!ledger.allocations && !ledger.bytes && !ledger.overflow);
}
/* Logical EXITED precedes the native OS-return tail. This independent
 * observer is used only after a late case has already proved timeout/ACK. */
static void blocking_finish_late(uint64_t deadline) {
  CHECK(blocking_driver.shutdown_deadline==deadline);
#if defined(_WIN32)
  CHECK(blocking_pool.workers_created==1u && blocking_driver.workers_created==1u);
  CHECK(WaitForSingleObject(blocking_pool.workers[0].thread,2000)==WAIT_OBJECT_0);
  CHECK(WaitForSingleObject(blocking_driver.workers[0].thread,2000)==WAIT_OBJECT_0);
#else
  alarm(2);
#endif
  CHECK(blocking_driver.shutdown_deadline==deadline);
  blocking_finish(deadline);
#if !defined(_WIN32)
  alarm(0);
#endif
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
static void blocking_queued_cancel_retires_before_occupied_worker_is_released(void) {
  blocking_setup(2u,4u*sizeof(KuTaskInstance_0)+1024u);fixture_gate_io=1;
  KuTaskValueV1 first=blocking_start("occupied.txt");CHECK(ku_test_event_wait(&fixture_io_entered,2000));
  KuTaskValueV1 queued=blocking_start("never-started.txt");blocking_idle();
  CHECK(blocking_pool_snapshot().queued==1u);
  CHECK(ku_task_driver_request_cancel(&queued.ticket,&queued.owner.lease,KU_TASK_CONTROL_CANCELLED,blocking_observation_deadline())==KU_TASK_CONTROL_OK);
  CHECK(ku_task_driver_wait_result(&queued.ticket,&queued.owner.lease,blocking_observation_deadline())==KU_TASK_DRIVER_WAIT_READY);
  CHECK(ku_task_control_status(&queued.owner.lease)==KU_TASK_CONTROL_CANCELLED);
  KuTaskBlockingSnapshotV1 snapshot=blocking_pool_snapshot();
  CHECK(snapshot.running==1u && snapshot.resident==1u && !snapshot.queued && snapshot.finished==1u);
  CHECK(fixture_io_calls==1u);blocking_no_file("never-started.txt");
  blocking_drop(&queued);blocking_idle();
  CHECK(ku_test_event_set(&fixture_io_release));blocking_take(&first,NULL);blocking_drop(&first);blocking_idle();
  CHECK(!blocking_pool_snapshot().resident && !blocking_driver_snapshot().reserved_bytes);
  CHECK(remove("occupied.txt")==0);blocking_finish(blocking_observation_deadline());
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
  CHECK(blocking_work_ledger().bytes==sizeof(KuTaskInstance_0)+copies);
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
  CHECK(remove("late.txt")==0); blocking_finish_late(deadline);
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
  CHECK(!blocking_work_ledger().allocations && !blocking_work_ledger().bytes && !blocking_work_ledger().overflow);
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
    CHECK(!blocking_driver_snapshot().reserved_bytes && !blocking_work_ledger().bytes);
  }
  CHECK(fixture_output_calls>4u);
  // Driver accounting must track live old+new capacities, not their historical sum.
  CHECK(blocking_driver_snapshot().peak_reserved_bytes<1600000u);
  CHECK(remove("read-input.txt")==0); blocking_finish(blocking_observation_deadline());
  for (size_t fail=0;fail<5;fail++) {
    blocking_setup(1,3u*1000000u+2u*sizeof(KuTaskInstance_2));
    blocking_read_file("read-fail.txt",45000u); fixture_fail_output_in=fail;
    KuTaskValueV1 task=blocking_read_start("read-fail.txt"); blocking_read_take(&task,0,"out_of_memory");
    blocking_drop(&task); blocking_idle(); CHECK(!blocking_work_ledger().bytes && !blocking_driver_snapshot().reserved_bytes);
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
  CHECK(remove("read-cancel.txt")==0); blocking_finish_late(deadline);
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
  CHECK(blocking_work_ledger().bytes==sizeof(KuTaskInstance_2));
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
    CHECK(blocking_work_ledger().allocations==1 && !fixture_io_calls);
    blocking_no_file("rejected.txt"); blocking_drop(&task); blocking_idle();
    KuTaskBlockingSnapshotV1 final=blocking_pool_snapshot();
    CHECK(!final.resident && !final.running && !final.queued && !final.accepted && !final.finished && final.rejected==1);
    blocking_finish(blocking_observation_deadline());
  }
}
static KuTaskValueV1 blocking_exists_start(KuString* path) {
  KuTaskValueV1 task={0}; CHECK(ku_task_3_start_value(&blocking_driver,path,&task)==KU_TASK_DRIVER_OK);
  CHECK(task.tag==KU_TASK_VALUE_LIVE&&!path->ptr&&!path->len&&!path->capacity); return task;
}
static KuTaskValueV1 blocking_exists_name(const char* name) {
  KuString path=ku_string_static((const uint8_t*)name,strlen(name)); return blocking_exists_start(&path);
}
static KuTaskValueV1 blocking_exists_owned(void) {
  uint8_t* bytes=(uint8_t*)malloc(31u); CHECK(bytes); memcpy(bytes,"exists-input.txt",16u);
  fixture_original_path=bytes; KuString path={bytes,16u,31u,KU_STRING_OWNED}; return blocking_exists_start(&path);
}
static void blocking_exists_take(KuTaskValueV1* task,bool present,const char* error_code) {
  CHECK(ku_task_driver_wait_result(&task->ticket,&task->owner.lease,blocking_observation_deadline())==KU_TASK_DRIVER_WAIT_READY);
  size_t calls=blocking_work_ledger().calls;
  KuTaskAdapterOutcomeV1 output={0}; CHECK(ku_task_value_take(task,NULL,&output)==KU_TASK_CONTROL_OK);
  CHECK(output.result_kind==2u&&output.value.boolean.ok==(error_code==NULL));
  if (error_code) {
    KuString code=output.value.boolean.error.code;
    CHECK(code.len==strlen(error_code)&&!memcmp(code.ptr,error_code,code.len));
    CHECK(output.exit_class==KU_TASK_EXIT_RUNTIME_FAILURE);
  } else {
    CHECK(output.value.boolean.value==present&&output.exit_class==KU_TASK_EXIT_USER_RESULT);
    CHECK(!output.has_cleanup_deadline&&!output.cleanup_deadline);
  }
  CHECK(blocking_work_ledger().calls==calls&&!fixture_output_calls);
  ku_task_outcome_drop(&output);
}
static void blocking_exists_values_and_infrastructure_errors(void) {
  blocking_setup(1,sizeof(KuTaskInstance_3)+1024u);
  blocking_read_file("exists-input.txt",1u);
  for (unsigned mode=0;mode<6u;mode++) {
    const uint8_t nul_path[]={'x',0,'y'},invalid_utf8[]={0xff};
    KuTaskValueV1 task;
    if (mode==0) task=blocking_exists_owned();
    else if (mode<3u) task=blocking_exists_name(mode==1 ? "." : "exists-missing.txt");
    else {
      KuString path=ku_string_static(mode==3u ? nul_path : mode==4u ? invalid_utf8 : (const uint8_t*)"",
          mode==3u ? sizeof(nul_path) : mode==4u ? sizeof(invalid_utf8) : 0u);
      task=blocking_exists_start(&path);
    }
    // Ku resolves an empty path to the current/base directory.
    blocking_exists_take(&task,mode<2u||mode==5u,NULL); blocking_idle();
    CHECK(blocking_driver_snapshot().reserved_bytes==sizeof(KuTaskInstance_3));
    CHECK(blocking_work_ledger().bytes==sizeof(KuTaskInstance_3));
    if (!mode) { CHECK(fixture_original_path_freed==1u); fixture_original_path=NULL; }
    blocking_drop(&task); blocking_idle(); CHECK(!blocking_work_ledger().bytes&&!blocking_driver_snapshot().reserved_bytes);
  }
  CHECK(!fixture_output_calls&&fixture_io_calls>=3u);
  CHECK(remove("exists-input.txt")==0); blocking_finish(blocking_observation_deadline());
  // Exists has one native path allocation and no content/output allocation.
  // Reuse the shared write/read fault hooks, without replaying their matrices.
  for (unsigned mode=0;mode<2u;mode++) {
    blocking_setup(1,sizeof(KuTaskInstance_3)+(mode ? 1024u : 0u));
    if (mode) fixture_fail_blocking_in=0;
    KuTaskValueV1 task=blocking_exists_name("exists-missing.txt");
    blocking_exists_take(&task,false,mode ? "out_of_memory" : "memory_limit");
    CHECK(!fixture_io_calls&&!fixture_output_calls&&blocking_work_ledger().allocations==1u);
    CHECK(blocking_driver_snapshot().reserved_bytes==sizeof(KuTaskInstance_3));
    KuTaskBlockingSnapshotV1 snapshot=blocking_pool_snapshot();
    CHECK(!snapshot.resident&&!snapshot.running&&!snapshot.queued&&!snapshot.accepted&&!snapshot.finished&&snapshot.rejected==1u);
    blocking_drop(&task); blocking_idle(); blocking_finish(blocking_observation_deadline());
  }
}
static void blocking_exists_real_job_does_not_block_timers_or_hide_queue_errors(void) {
  blocking_setup(2,4u*(sizeof(KuTaskInstance_3)+sizeof(KuTaskInstance_1))+2048u);
  blocking_read_file("exists-input.txt",1u); fixture_gate_io=1;
  KuTaskValueV1 running=blocking_exists_owned(); CHECK(ku_test_event_wait(&fixture_io_entered,2000)); blocking_idle();
  KuTaskInstance_3* instance=(KuTaskInstance_3*)running.owner.lease.control->context;
  KuTaskBlockingTokenV1 token=instance->blocking;
  CHECK(instance->frame.s_0.ptr==fixture_original_path&&instance->frame.s_0.len==16u&&instance->frame.s_0.capacity==31u);
  CHECK(!fixture_original_path_freed&&token.pool&&token.charged_bytes);
  CHECK(!ku_task_blocking_lock(&blocking_pool));
  CHECK(blocking_pool.jobs[token.slot].kind==3u&&!blocking_pool.jobs[token.slot].output_charge);
  ku_task_blocking_unlock(&blocking_pool);
  CHECK(blocking_driver_snapshot().reserved_bytes==sizeof(KuTaskInstance_3)+31u+token.charged_bytes);
  CHECK(blocking_work_ledger().bytes==sizeof(KuTaskInstance_3)+31u+token.charged_bytes);
  KuTaskValueV1 timer={0}; CHECK(ku_task_1_start_value(&blocking_driver,&timer)==KU_TASK_DRIVER_OK);
  blocking_take(&timer,NULL); blocking_drop(&timer); blocking_idle();
  CHECK(blocking_pool_snapshot().running==1u&&!fixture_output_calls); // Sole ordinary worker ran the timer.
  KuTaskValueV1 queued=blocking_exists_name("exists-missing.txt"); blocking_idle();
  CHECK(blocking_pool_snapshot().running==1u&&blocking_pool_snapshot().queued==1u);
  KuTaskValueV1 full=blocking_exists_name("exists-missing.txt"); blocking_exists_take(&full,false,"blocking_queue_full"); blocking_drop(&full);
  CHECK(ku_task_blocking_close(&blocking_pool)==KU_TASK_DRIVER_OK);
  KuTaskValueV1 closed=blocking_exists_name("exists-missing.txt"); blocking_exists_take(&closed,false,"blocking_pool_stopped"); blocking_drop(&closed);
  CHECK(ku_test_event_set(&fixture_io_release));
  blocking_exists_take(&running,true,NULL); blocking_exists_take(&queued,false,"blocking_pool_stopped");
  blocking_drop(&running); blocking_drop(&queued); blocking_idle();
  KuTaskBlockingSnapshotV1 snapshot=blocking_pool_snapshot();
  CHECK(snapshot.accepted==2u&&snapshot.finished==2u&&snapshot.rejected==2u&&snapshot.rejected_queue==1u);
  CHECK(!snapshot.resident&&!snapshot.running&&!snapshot.queued&&!snapshot.fault);
  CHECK(fixture_io_calls==1u&&!fixture_output_calls&&fixture_original_path_freed==1u);
  CHECK(!blocking_work_ledger().bytes&&!blocking_driver_snapshot().reserved_bytes);
  CHECK(remove("exists-input.txt")==0); blocking_finish(blocking_observation_deadline());
}
static void blocking_exists_cancel_retires_actual_job_with_original_deadline(void) {
  blocking_setup(1,2u*sizeof(KuTaskInstance_3)+1024u);
  blocking_read_file("exists-input.txt",1u); fixture_gate_io=1;
  KuTaskValueV1 task=blocking_exists_owned(); CHECK(ku_test_event_wait(&fixture_io_entered,2000)); blocking_idle();
  KuTaskInstance_3* instance=(KuTaskInstance_3*)task.owner.lease.control->context;
  size_t copies=instance->blocking.charged_bytes; CHECK(copies&&!fixture_original_path_freed&&!fixture_output_calls);
  KuTaskControlLeaseV1 late={0}; CHECK(ku_task_control_lease_retain(&task.owner.lease,&late)==KU_TASK_CONTROL_OK);
  uint64_t deadline=ku_task_driver_now_ms()+30u; KuTaskDriverCleanupReceiptV1 receipt={0};
  CHECK(ku_task_driver_owner_drop_receipt(&task.ticket,&task.owner,deadline,&receipt)==KU_TASK_DRIVER_OK);
  task=(KuTaskValueV1){0}; blocking_idle();
  CHECK(fixture_original_path_freed==1u&&ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_PENDING);
  CHECK(blocking_driver_snapshot().reserved_bytes==sizeof(KuTaskInstance_3)+copies);
  CHECK(blocking_work_ledger().bytes==sizeof(KuTaskInstance_3)+copies);
  CHECK(ku_task_driver_shutdown(&blocking_driver,deadline)==KU_TASK_DRIVER_SHUTDOWN_TIMEOUT);
  CHECK(ku_task_blocking_finish(&blocking_pool,deadline)==KU_TASK_DRIVER_SHUTDOWN_TIMEOUT);
  CHECK(ku_task_driver_destroy(&blocking_driver)==KU_TASK_DRIVER_PENDING);
  CHECK(blocking_pool_snapshot().running==1u&&!blocking_pool_snapshot().finished);
  CHECK(ku_task_control_cleanup_deadline(late.control)==deadline);
  CHECK(ku_test_event_set(&fixture_io_release));
  CHECK(!ku_task_blocking_lock(&blocking_pool)); uint64_t observer=blocking_observation_deadline();
  while (blocking_pool.finished!=1u) CHECK(ku_task_blocking_wait(&blocking_pool,observer)==0);
  ku_task_blocking_unlock(&blocking_pool); blocking_idle();
  CHECK(ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_CLEANUP_ACK);
  CHECK(ku_task_control_status(&late)==KU_TASK_CONTROL_CANCELLED&&ku_task_control_cleanup_deadline(late.control)==deadline);
  CHECK(!blocking_pool_snapshot().resident&&!fixture_output_calls&&fixture_io_calls==1u);
  CHECK(blocking_driver_snapshot().reserved_bytes==sizeof(KuTaskInstance_3)&&blocking_work_ledger().bytes==sizeof(KuTaskInstance_3));
  CHECK(ku_task_control_lease_release(&late)==KU_TASK_CONTROL_OK); blocking_idle();
  CHECK(!ku_task_blocking_lock(&blocking_pool)); observer=blocking_observation_deadline();
  while (blocking_pool.workers_exited!=blocking_pool.workers_created) CHECK(ku_task_blocking_wait(&blocking_pool,observer)==0);
  ku_task_blocking_unlock(&blocking_pool);
  CHECK(!blocking_driver_snapshot().resident&&!blocking_driver_snapshot().reserved_bytes);
  CHECK(remove("exists-input.txt")==0);
  blocking_finish_late(deadline);
}

static KuTaskValueV1 blocking_directory_start(const char* name) {
  KuString path=ku_string_static((const uint8_t*)name,strlen(name)); KuTaskValueV1 task={0};
  CHECK(ku_task_4_start_value(&blocking_driver,&path,&task)==KU_TASK_DRIVER_OK);
  CHECK(task.tag==KU_TASK_VALUE_LIVE); return task;
}
static void blocking_directory_take(KuTaskValueV1* task,size_t count,const char* code) {
  CHECK(ku_task_driver_wait_result(&task->ticket,&task->owner.lease,blocking_observation_deadline())==KU_TASK_DRIVER_WAIT_READY);
  KuTaskAdapterOutcomeV1 output={0}; CHECK(ku_task_value_take(task,NULL,&output)==KU_TASK_CONTROL_OK);
  CHECK(output.result_kind==7u && output.value.string_array.ok==(code==NULL));
  if (code) {
    KuString got=output.value.string_array.error.code;
    CHECK(got.len==strlen(code)&&!memcmp(got.ptr,code,got.len));
  } else {
    KuArray_str values=output.value.string_array.value;
    CHECK(values.len==count&&values.capacity>=count);
    for (size_t i=1;i<values.len;i++) CHECK(ku_fs_path_compare(&values.data[i-1],&values.data[i])<0);
    CHECK(blocking_driver_snapshot().reserved_bytes==sizeof(KuTaskInstance_4));
    if (count>64u) {
      /* Moving a real large listing through another Task exercises the sorted
       * Start scratch checks, and failed validation must preserve its owner. */
      KuArray_str original=values; KuTaskValueV1 carried={0};
      CHECK(ku_task_5_start_value(&blocking_driver,&values,&carried)==KU_TASK_DRIVER_OK&&!values.data);
      output.value.string_array.value=(KuArray_str){0};
      CHECK(ku_task_driver_wait_result(&carried.ticket,&carried.owner.lease,blocking_observation_deadline())==KU_TASK_DRIVER_WAIT_READY);
      KuTaskAdapterOutcomeV1 copied={0}; CHECK(ku_task_value_take(&carried,NULL,&copied)==KU_TASK_CONTROL_OK);
      CHECK(copied.value.string_array.ok&&copied.value.string_array.value.data==original.data);
      ku_task_outcome_drop(&copied); CHECK(ku_task_value_drop(&carried,blocking_observation_deadline())==KU_TASK_DRIVER_OK);
    }
  }
  ku_task_outcome_drop(&output);
}
static void blocking_directory_values_and_failures(void) {
  for (unsigned mode=0;mode<3u;mode++) {
    blocking_setup(2u,2u*1024u*1024u);
    KuTaskValueV1 task=blocking_directory_start(mode==0 ? "listing" : mode==1 ? "empty" : "missing");
    blocking_directory_take(&task,mode==0 ? 80u : 0u,mode==2 ? "read_dir_failed" : NULL);
    blocking_drop(&task); blocking_idle();
    CHECK(!blocking_driver_snapshot().reserved_bytes&&!blocking_pool_snapshot().resident);
    blocking_finish(blocking_observation_deadline());
  }
  for (size_t fail=0;fail<12u;fail++) {
    blocking_setup(2u,2u*1024u*1024u); fixture_fail_output_in=fail;
    KuTaskValueV1 task=blocking_directory_start("listing");
    blocking_directory_take(&task,0,"out_of_memory"); blocking_drop(&task); blocking_idle();
    CHECK(!blocking_driver_snapshot().reserved_bytes&&!blocking_pool_snapshot().resident);
    blocking_finish(blocking_observation_deadline());
  }
  blocking_setup(2u,sizeof(KuTaskInstance_4)+256u);
  KuTaskValueV1 limited=blocking_directory_start("listing");
  blocking_directory_take(&limited,0,"memory_limit"); blocking_drop(&limited); blocking_idle();
  CHECK(!blocking_driver_snapshot().reserved_bytes); blocking_finish(blocking_observation_deadline());
}
static void blocking_directory_real_wait_and_cancel(void) {
  blocking_setup(2u,2u*1024u*1024u); fixture_gate_io=1;
  KuTaskValueV1 task=blocking_directory_start("listing");
  CHECK(ku_test_event_wait(&fixture_io_entered,2000)); blocking_idle();
  KuTaskValueV1 tick={0}; CHECK(ku_task_1_start_value(&blocking_driver,&tick)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_wait_result(&tick.ticket,&tick.owner.lease,blocking_observation_deadline())==KU_TASK_DRIVER_WAIT_READY);
  KuTaskAdapterOutcomeV1 output={0}; CHECK(ku_task_value_take(&tick,NULL,&output)==KU_TASK_CONTROL_OK&&output.value.null_value.ok);
  ku_task_outcome_drop(&output); blocking_drop(&tick);
  KuTaskControlLeaseV1 late={0}; CHECK(ku_task_control_lease_retain(&task.owner.lease,&late)==KU_TASK_CONTROL_OK);
  uint64_t deadline=ku_task_driver_now_ms()+30u;
  KuTaskDriverCleanupReceiptV1 receipt={0};
  CHECK(ku_task_driver_owner_drop_receipt(&task.ticket,&task.owner,deadline,&receipt)==KU_TASK_DRIVER_OK);
  task=(KuTaskValueV1){0}; blocking_idle();
  CHECK(ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_PENDING);
  CHECK(blocking_pool_snapshot().resident==1u&&blocking_driver_snapshot().reserved_bytes>=sizeof(KuTaskInstance_4));
  CHECK(ku_test_event_set(&fixture_io_release));
  uint64_t observer=blocking_observation_deadline();
  CHECK(!ku_task_blocking_lock(&blocking_pool));
  while (blocking_pool.running||blocking_pool.queued) CHECK(!ku_task_blocking_wait(&blocking_pool,observer));
  ku_task_blocking_unlock(&blocking_pool); blocking_idle();
  CHECK(ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_CLEANUP_ACK);
  CHECK(ku_task_control_cleanup_deadline(late.control)==deadline);
  CHECK(!blocking_pool_snapshot().resident&&blocking_driver_snapshot().reserved_bytes==sizeof(KuTaskInstance_4));
  CHECK(ku_task_control_lease_release(&late)==KU_TASK_CONTROL_OK); blocking_idle();
  CHECK(!blocking_driver_snapshot().reserved_bytes); blocking_finish(blocking_observation_deadline());
}
static void blocking_capacity_storage_is_bounded_and_freed(void) {
  KuTaskBlockingPoolV1 pool={0};
  CHECK(KU_TASK_DRIVER_MAX_SLOTS>=16384u);
  CHECK(ku_task_blocking_fixed_bytes(SIZE_MAX)==SIZE_MAX);
  KuTaskBlockingJobV1 layout={0};
  CHECK((void*)&layout.result==(void*)&layout.read_result);
  CHECK((void*)&layout.result==(void*)&layout.directory_result);
  fprintf(stderr,"file-job-layout job=%zu result_members_sum=%zu fixed_1024=%zu fixed_16384=%zu\n",
      sizeof(layout),sizeof(layout.result)+sizeof(layout.read_result)+sizeof(layout.directory_result),
      ku_task_blocking_fixed_bytes(1024u),ku_task_blocking_fixed_bytes(16384u));
  CHECK(ku_task_blocking_init(&pool,16385u,1u,blocking_observation_deadline())==KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(!fixture_ledger().bytes && !pool.initialized);
  /* Allocation failure leaves a retryable, explicitly reapable partial pool. */
  fixture_fail_pool_storage=1;
  CHECK(ku_task_blocking_init(&pool,16384u,1u,blocking_observation_deadline())==KU_TASK_BLOCKING_OUT_OF_MEMORY);
  CHECK(pool.initialized==KU_TASK_DRIVER_STORAGE_REAPING && !pool.storage);
  CHECK(ku_task_blocking_finish(&pool,blocking_observation_deadline())==KU_TASK_DRIVER_OK);
  CHECK(!pool.initialized && !pool.storage_bytes && !fixture_ledger().bytes);
  for(size_t cap=1;cap<=16384u;cap=cap==1u?16384u:16385u) {
    memset(&pool,0,sizeof(pool));
    CHECK(ku_task_blocking_init(&pool,cap,1u,blocking_observation_deadline())==KU_TASK_DRIVER_OK);
    size_t bytes=cap*(sizeof(KuTaskBlockingJobV1)+sizeof(size_t));
    CHECK(pool.storage_bytes==bytes && pool.capacity==cap);
    CHECK((void*)pool.jobs==pool.storage && pool.ring==(size_t*)((uint8_t*)pool.storage+cap*sizeof(*pool.jobs)));
    CHECK(pool.workers_created==1u && fixture_ledger().allocations==1u && fixture_ledger().bytes==bytes);
    CHECK(ku_task_blocking_finish(&pool,blocking_observation_deadline())==KU_TASK_DRIVER_OK);
    CHECK(!pool.initialized && !pool.storage && !pool.jobs && !pool.ring);
    CHECK(!fixture_ledger().allocations && !fixture_ledger().bytes);
  }
}
int main(void) {
  CHECK(KU_TASK_FRAME_ABI_VERSION==5u && KU_TASK_CONTROL_ABI_VERSION==2u && KU_TASK_DRIVER_ABI_VERSION==11u);
  blocking_capacity_storage_is_bounded_and_freed();
  blocking_queue_and_generation(); blocking_queued_cancel_retires_before_occupied_worker_is_released();
  blocking_late_retirement_keeps_original_deadline();
  blocking_rejections_have_no_file_effect(); blocking_close_skips_queued_files();
  blocking_fault_is_reported_after_real_resource_cleanup();
  blocking_read_boundaries_and_failures(); blocking_read_cancel_holds_real_output_until_done();
  blocking_read_done_cancellation_drops_owned_output();
  blocking_exists_values_and_infrastructure_errors();
  blocking_exists_real_job_does_not_block_timers_or_hide_queue_errors();
  blocking_exists_cancel_retires_actual_job_with_original_deadline();
  blocking_directory_values_and_failures(); blocking_directory_real_wait_and_cancel();
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
  CHECK(KU_TASK_FRAME_ABI_VERSION==5u&&KU_TASK_CONTROL_ABI_VERSION==2u&&KU_TASK_DRIVER_ABI_VERSION==11u);
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

#[test]
fn native_task_flat_array_results_keep_staged_owners_and_release_replaced_or_cancelled_payloads() {
    let source = r#"
async fn Choose(value: [str], replacement: [str], mode: int): [str]! {
    try {
        if (mode==3) { fail "before" }
        return ok(value)
    } finally {
        println("gate")
        if (mode==1) { return ok(replacement) }
        if (mode==2) { fail "replacement" }
    }
    empty: [str]=[]
    return ok(empty)
}
async fn Broken(): str! { fail "partial" }
async fn Partial(first: str): [str]! { return ok([first,(await Broken())?]) }
async fn main(): null! { return ok(null) }
"#;
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let native = ir::task_lower::lower_program(&ast).unwrap();
    let function = &native.tasks.functions[0];
    assert_eq!(function.name, "Choose");
    let input = function.parameters[0];
    let wrapped: Vec<_> = function
        .states
        .iter()
        .flat_map(|state| &state.operations)
        .filter_map(|operation| match operation {
            TaskOp::WrapOk { dst, src } if *src == input => Some(*dst),
            _ => None,
        })
        .collect();
    assert_eq!(wrapped.len(), 1, "one primary array return");
    let saved: Vec<_> = function
        .states
        .iter()
        .flat_map(|state| &state.operations)
        .filter_map(|operation| match operation {
            TaskOp::Move { dst, src } if *src == wrapped[0] => Some(*dst),
            _ => None,
        })
        .collect();
    assert_eq!(saved.len(), 1, "finally preserves one owned return slot");
    let hooks = r#"
static KuTestEvent result_entered,result_proceed;
static unsigned result_gate_calls;
static int result_block;
static void* result_instance;
static size_t result_write(const void*,size_t,size_t,FILE*);
#define fwrite result_write
"#;
    let mut generated = finally_adapter_generated(source, hooks);
    let resume = "static uint32_t ku_task_0_resume(void* raw) {";
    assert_eq!(generated.matches(resume).count(), 1);
    generated = generated.replacen(resume, &format!("{resume}\n  result_instance=raw;"), 1);
    generated.push_str(
        &ARRAY_RESULT_MAIN
            .replace("@INPUT@", &input.0.to_string())
            .replace("@SAVED@", &saved[0].0.to_string()),
    );
    run_finally_adapter_generated(
        generated,
        "native-task-flat-array-results",
        "\n\n\n\n\n\narray-result-owners-ok\n",
    );
}

const ARRAY_RESULT_MAIN: &str = r#"
typedef struct ResultRuntime { KuTaskDriverV1 driver; KuTaskDriverSlotV1 slots[4]; size_t ring[4]; } ResultRuntime;
static uintptr_t result_original,result_replacement;
static size_t result_original_charge,result_replacement_charge;
static uint64_t result_deadline(void) { return ku_task_driver_now_ms()+2000u; }
static KuTaskDriverSnapshotV1 result_snapshot(ResultRuntime* runtime) {
  KuTaskDriverSnapshotV1 snapshot={0}; CHECK(ku_task_driver_snapshot(&runtime->driver,&snapshot)==KU_TASK_DRIVER_OK); return snapshot;
}
static void result_idle(ResultRuntime* runtime) {
  CHECK(ku_task_driver_wait_idle(&runtime->driver,result_deadline())==KU_TASK_DRIVER_OK);
}
static size_t result_write(const void* bytes,size_t size,size_t count,FILE* stream) {
  (void)stream;
  if (size==1u && count==4u && !memcmp(bytes,"gate",4u)) {
    CHECK(!result_gate_calls++);
    if (result_block) {
      KuTaskInstance_0* instance=(KuTaskInstance_0*)result_instance;
      // Ordinary finally runs while the return is in its dedicated owned
      // frame slot. Exit stages frame.result only after this body finishes.
      CHECK(instance && !instance->frame.header.result_initialized && !instance->payload_initialized);
      CHECK(instance->frame.finally_unwind_active);
      CHECK(instance->frame.header.initialized&(UINT64_C(1)<<@SAVED@));
      CHECK(!(instance->frame.header.initialized&(UINT64_C(1)<<@INPUT@)));
      CHECK(!instance->frame.s_@INPUT@.data&&!instance->frame.s_@INPUT@.len&&!instance->frame.s_@INPUT@.capacity);
      CHECK(instance->frame.s_@SAVED@.ok && (uintptr_t)instance->frame.s_@SAVED@.value.data==result_original);
      CHECK(instance->frame.s_@SAVED@.value.capacity==4u && instance->frame.s_@SAVED@.value.len==2u);
      CHECK(ku_test_event_set(&result_entered)); CHECK(ku_test_event_wait(&result_proceed,2000));
    }
  }
  return count;
}
static void result_init(ResultRuntime* runtime) {
  CHECK(!fixture_ledger().allocations&&!fixture_ledger().bytes);
  memset(runtime,0,sizeof(*runtime)); result_gate_calls=0; result_block=0; result_instance=NULL;
  CHECK(ku_test_event_init(&result_entered)); CHECK(ku_test_event_init(&result_proceed));
  size_t fixed=sizeof(runtime->driver)+sizeof(runtime->slots)+sizeof(runtime->ring);
  CHECK(ku_task_driver_init(&runtime->driver,sizeof(runtime->driver),KU_TASK_DRIVER_ABI_VERSION,
      runtime->slots,4u,runtime->ring,4u,fixed+sizeof(KuTaskInstance_0)+sizeof(KuTaskInstance_1)+sizeof(KuTaskInstance_2)+4096u,
      1u,result_deadline())==KU_TASK_DRIVER_OK);
}
static KuArray_str result_input(int replacement) {
  KuArray_str value={replacement ? 1u : 2u,NULL,replacement ? 2u : 4u};
  value.data=(KuString*)calloc(value.capacity,sizeof(*value.data)); CHECK(value.data);
  size_t capacity=replacement ? 37u : 31u;
  uint8_t* leaf=(uint8_t*)malloc(capacity); CHECK(leaf);
  memcpy(leaf,replacement ? "second" : "first",replacement ? 6u : 5u);
  value.data[0]=(KuString){leaf,replacement ? 6u : 5u,capacity,KU_STRING_OWNED};
  size_t charge=value.capacity*sizeof(KuString)+capacity;
  if (replacement) { result_replacement=(uintptr_t)value.data; result_replacement_charge=charge; }
  else {
    uint8_t* empty=(uint8_t*)malloc(1u); CHECK(empty); empty[0]=0;
    value.data[1]=(KuString){empty,0u,0u,KU_STRING_OWNED};
    result_original=(uintptr_t)value.data; result_original_charge=charge+1u;
  }
  return value;
}
static KuTaskValueV1 result_start(ResultRuntime* runtime,int64_t mode) {
  KuArray_str value=result_input(0),replacement=result_input(1); KuTaskValueV1 task={0};
  CHECK(ku_task_0_start_value(&runtime->driver,&value,&replacement,&mode,&task)==KU_TASK_DRIVER_OK);
  CHECK(task.tag==KU_TASK_VALUE_LIVE&&!value.data&&!value.len&&!value.capacity&&!replacement.data&&!replacement.len&&!replacement.capacity);
  return task;
}
static void result_finish(ResultRuntime* runtime) {
  result_idle(runtime); KuTaskDriverSnapshotV1 snapshot=result_snapshot(runtime);
  CHECK(!snapshot.resident&&!snapshot.reserved_bytes&&!snapshot.fault);
  uint64_t deadline=runtime->driver.closing ? runtime->driver.shutdown_deadline : result_deadline();
  CHECK(ku_task_driver_shutdown(&runtime->driver,deadline)==KU_TASK_DRIVER_OK);
  uint64_t effective=runtime->driver.shutdown_deadline; snapshot=result_snapshot(runtime);
  CHECK(snapshot.workers_created==1u&&snapshot.workers_exited==1u&&!snapshot.workers_joined
      &&!snapshot.workers_waiting&&!snapshot.resident&&!snapshot.reserved_bytes&&!snapshot.running&&!snapshot.queued);
#if defined(_WIN32)
  // Logical EXITED precedes native handle signalling. Observe that tail with
  // the existing OS bound, then join using the original D without renewal.
  if (ku_task_driver_now_ms()>=effective)
    CHECK(WaitForSingleObject(runtime->driver.workers[0].thread,2000)==WAIT_OBJECT_0);
#else
  alarm(2);
#endif
  CHECK(ku_task_driver_join(&runtime->driver,effective)==KU_TASK_DRIVER_OK);
  CHECK(runtime->driver.shutdown_deadline==effective&&runtime->driver.workers_joined==1u
      &&runtime->driver.workers[0].joined&&runtime->driver.workers[0].closed);
  CHECK(ku_task_driver_destroy(&runtime->driver)==KU_TASK_DRIVER_OK);
#if !defined(_WIN32)
  alarm(0);
#endif
  CHECK(ku_test_event_destroy(&result_entered)); CHECK(ku_test_event_destroy(&result_proceed));
  CHECK(!fixture_ledger().allocations&&!fixture_ledger().bytes&&!fixture_ledger().overflow);
}
static void result_case(unsigned mode) {
  ResultRuntime runtime; result_init(&runtime);
  KuTaskValueV1 task=result_start(&runtime,mode==4u ? 0 : (int64_t)mode);
  CHECK(ku_task_driver_wait_result(&task.ticket,&task.owner.lease,result_deadline())==KU_TASK_DRIVER_WAIT_READY);
  result_idle(&runtime); CHECK(result_gate_calls==1u);
  const size_t active=mode==1u ? result_replacement_charge : mode==2u||mode==3u ? 0u : result_original_charge;
  CHECK(result_snapshot(&runtime).resident==1u&&result_snapshot(&runtime).reserved_bytes==sizeof(KuTaskInstance_0)+active);
  CHECK(fixture_ledger().bytes==sizeof(KuTaskInstance_0)+active);
  KuTaskControlLeaseV1 late={0}; CHECK(ku_task_control_lease_retain(&task.owner.lease,&late)==KU_TASK_CONTROL_OK);
  if (mode==4u) {
    // Untaken published arrays are dropped by the real payload destructor.
    CHECK(ku_task_value_drop(&task,result_deadline())==KU_TASK_DRIVER_OK); result_idle(&runtime);
  } else {
    KuTaskInstance_0* instance=(KuTaskInstance_0*)task.owner.lease.control->context;
    if (mode==2u||mode==3u) {
      CHECK(instance->payload_initialized&&!instance->payload.ok);
      // An Err's inactive array must never be validated, scanned or freed.
      instance->payload.value=(KuArray_str){SIZE_MAX,(KuString*)(uintptr_t)1u,SIZE_MAX};
    }
    size_t calls=fixture_ledger().calls;
    KuTaskAdapterOutcomeV1 output={0}; CHECK(ku_task_value_take(&task,NULL,&output)==KU_TASK_CONTROL_OK);
    CHECK(output.result_kind==7u&&output.value.string_array.ok==(mode<2u));
    CHECK(output.exit_class==KU_TASK_EXIT_USER_RESULT&&!output.has_cleanup_deadline&&!output.cleanup_deadline);
    CHECK(fixture_ledger().calls==calls&&result_snapshot(&runtime).reserved_bytes==sizeof(KuTaskInstance_0));
    if (mode<2u) {
      KuArray_str value=output.value.string_array.value;
      CHECK((uintptr_t)value.data==(mode ? result_replacement : result_original));
      CHECK(value.capacity==(mode ? 2u : 4u)&&value.len==(mode ? 1u : 2u));
      CHECK(value.data[0].storage==KU_STRING_OWNED&&value.data[0].capacity==(mode ? 37u : 31u));
      CHECK(value.data[0].len==(mode ? 6u : 5u)&&!memcmp(value.data[0].ptr,mode ? "second" : "first",value.data[0].len));
      if (!mode) CHECK(value.data[1].storage==KU_STRING_OWNED&&!value.data[1].len&&!value.data[1].capacity&&!value.data[1].ptr[0]);
      CHECK(fixture_ledger().bytes==sizeof(KuTaskInstance_0)+active);
    } else {
      KuString message=output.value.string_array.error.message;
      const char* expected=mode==2u ? "replacement" : "before";
      CHECK(message.len==strlen(expected)&&!memcmp(message.ptr,expected,message.len));
    }
    ku_task_outcome_drop(&output); CHECK(ku_task_outcome_empty(&output,7u));
    CHECK(fixture_ledger().bytes==sizeof(KuTaskInstance_0));
    CHECK(ku_task_value_take(&task,NULL,&output)==KU_TASK_CONTROL_RESULT_TAKEN);
    CHECK(ku_task_value_drop(&task,result_deadline())==KU_TASK_DRIVER_OK); result_idle(&runtime);
  }
  CHECK(result_snapshot(&runtime).resident==1u&&result_snapshot(&runtime).reserved_bytes==sizeof(KuTaskInstance_0));
  CHECK(fixture_ledger().bytes==sizeof(KuTaskInstance_0));
  CHECK(late.control->frame_destroyed&&!ku_task_control_atomic_load(&late.control->lifecycle_pin));
  CHECK(ku_task_control_lease_release(&late)==KU_TASK_CONTROL_OK); result_finish(&runtime);
}
static void result_partial(void) {
  ResultRuntime runtime; result_init(&runtime);
  uint8_t* bytes=(uint8_t*)malloc(31u); CHECK(bytes); memcpy(bytes,"first",5u);
  KuString first={bytes,5u,31u,KU_STRING_OWNED}; KuTaskValueV1 task={0};
  size_t calls=fixture_ledger().calls;
  CHECK(ku_task_2_start_value(&runtime.driver,&first,&task)==KU_TASK_DRIVER_OK&&!first.ptr);
  CHECK(ku_task_driver_wait_result(&task.ticket,&task.owner.lease,result_deadline())==KU_TASK_DRIVER_WAIT_READY);
  result_idle(&runtime);
  CHECK(fixture_ledger().calls==calls+2u); // Parent and failed child; ArrayMake was never reached.
  CHECK(result_snapshot(&runtime).resident==1u&&result_snapshot(&runtime).reserved_bytes==sizeof(KuTaskInstance_2));
  CHECK(fixture_ledger().bytes==sizeof(KuTaskInstance_2));
  KuTaskAdapterOutcomeV1 output={0}; CHECK(ku_task_value_take(&task,NULL,&output)==KU_TASK_CONTROL_OK);
  CHECK(output.result_kind==7u&&!output.value.string_array.ok);
  CHECK(output.value.string_array.error.message.len==7u&&!memcmp(output.value.string_array.error.message.ptr,"partial",7u));
  ku_task_outcome_drop(&output); CHECK(ku_task_value_drop(&task,result_deadline())==KU_TASK_DRIVER_OK);
  result_finish(&runtime);
}
static void result_cancel_staged(void) {
  ResultRuntime runtime; result_init(&runtime); result_block=1;
  KuTaskValueV1 task=result_start(&runtime,0);
  CHECK(ku_test_event_wait(&result_entered,2000));
  CHECK(fixture_ledger().bytes==sizeof(KuTaskInstance_0)+result_original_charge+result_replacement_charge);
  KuTaskControlLeaseV1 late={0}; CHECK(ku_task_control_lease_retain(&task.owner.lease,&late)==KU_TASK_CONTROL_OK);
  uint64_t deadline=ku_task_driver_now_ms()+30u; KuTaskDriverCleanupReceiptV1 receipt={0};
  CHECK(ku_task_driver_owner_drop_receipt(&task.ticket,&task.owner,deadline,&receipt)==KU_TASK_DRIVER_OK); task=(KuTaskValueV1){0};
  CHECK(ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_PENDING);
  CHECK(ku_task_driver_shutdown(&runtime.driver,deadline)==KU_TASK_DRIVER_SHUTDOWN_TIMEOUT);
  CHECK(ku_task_control_cleanup_deadline(late.control)==deadline);
  CHECK(ku_test_event_set(&result_proceed)); result_idle(&runtime);
  CHECK(ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_CLEANUP_ACK&&result_gate_calls==1u);
  CHECK(ku_task_control_cleanup_deadline(late.control)==deadline);
  CHECK(result_snapshot(&runtime).resident==1u&&result_snapshot(&runtime).reserved_bytes==sizeof(KuTaskInstance_0));
  CHECK(fixture_ledger().bytes==sizeof(KuTaskInstance_0));
  CHECK(ku_task_control_lease_release(&late)==KU_TASK_CONTROL_OK); result_finish(&runtime);
}
int main(void) {
  CHECK(KU_TASK_FRAME_ABI_VERSION==5u&&KU_TASK_CONTROL_ABI_VERSION==2u&&KU_TASK_DRIVER_ABI_VERSION==11u);
  for (unsigned mode=0;mode<5u;mode++) result_case(mode);
  result_partial(); result_cancel_staged();
  puts("array-result-owners-ok"); return 0;
}
"#;

#[test]
fn native_task_owned_reassignment_replaces_only_after_rhs_and_retires_cancelled_children() {
    let source = r#"
async fn Incoming(value: str, stop: bool): str! {
    println("rhs")
    if (stop) { fail "rhs failed" }
    return ok(value)
}
async fn Assign(old: str, replacement: str, mode: int): str! {
    value=old
    value=value
    if (mode==2) {
        value=(await Incoming(value,true))?
    } else {
        value=(await Incoming(replacement,mode==1))?
    }
    return ok(value)
}
async fn AssignArray(old: [str], replacement: [str]): [str]! {
    value=old
    value=replacement
    value=value
    return ok(value)
}
async fn AssignSaved(old: [str], replacement: [str]): [str]! {
    value: [str]! = ok(old)
    value=ok(replacement)
    value=value
    return value
}
async fn main(): null! { return ok(null) }
"#;
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let mut native = ir::task_lower::lower_program(&ast).unwrap();
    let parent = &native.tasks.functions[1];
    assert_eq!(parent.name, "Assign");
    let initial: Vec<_> = parent
        .states
        .iter()
        .flat_map(|state| &state.operations)
        .filter_map(|operation| match operation {
            TaskOp::Move { dst, src } if *src == parent.parameters[0] => Some(*dst),
            _ => None,
        })
        .collect();
    assert_eq!(initial.len(), 1, "one original owned local");
    // This legacy Complete graph has no source Exit/Task/array admission to
    // select hosted storage accidentally. Planner and C emitter must agree
    // that the owned replacement itself requires persistent fee projection.
    let raw_id = native.tasks.functions.len();
    assert_eq!(raw_id, 5);
    native.tasks.functions.push(TaskFunction {
        id: TaskFunctionId(raw_id),
        name: "RawOwnedReplace".into(),
        slots: vec![
            slot(IrType::Str),
            slot(IrType::Str),
            slot(result(IrType::Str)),
        ],
        parameters: vec![SlotId(0), SlotId(1)],
        entry: StateId(0),
        result: result(IrType::Str),
        states: vec![TaskState {
            operations: vec![
                TaskOp::DropIfInit { slot: SlotId(0) },
                TaskOp::Move {
                    dst: SlotId(0),
                    src: SlotId(1),
                },
                TaskOp::WrapOk {
                    dst: SlotId(2),
                    src: SlotId(0),
                },
            ],
            terminator: TaskTerminator::Complete { value: SlotId(2) },
        }],
    });
    let hooks = r#"
static KuTestEvent assignment_entered,assignment_proceed;
static void *assignment_parent,*assignment_child;
static uintptr_t assignment_pointers[5];
static size_t assignment_frees[5];
static unsigned assignment_gate_calls;
static void assignment_free(void*);
static size_t assignment_write(const void*,size_t,size_t,FILE*);
#undef free
#define free assignment_free
#define fwrite assignment_write
"#;
    let generated = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
    let instrumentation = format!(
        "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{}\n{ALLOCATION_HOOK}\n{}\n{hooks}",
        native_task_ledger::LEDGER_LOCK,
        native_task_ledger::LOCKED_ALLOCATIONS
    );
    assert_eq!(generated.matches("typedef struct KuString {").count(), 1);
    let generated = generated.replacen(
        "typedef struct KuString {",
        &format!("{instrumentation}typedef struct KuString {{"),
        1,
    );
    assert_eq!(generated.matches("int main(void) {").count(), 1);
    let mut generated = generated.replacen(
        "int main(void) {",
        "static int fixture_unmodified_assignment_main(void) {",
        1,
    );
    for (id, name) in [(0, "child"), (1, "parent")] {
        let resume = format!("static uint32_t ku_task_{id}_resume(void* raw) {{");
        assert_eq!(generated.matches(&resume).count(), 1);
        generated = generated.replacen(&resume, &format!("{resume}\n  assignment_{name}=raw;"), 1);
    }
    generated.push_str(&OWNED_ASSIGNMENT_MAIN.replace("@VALUE@", &initial[0].0.to_string()));
    run_finally_adapter_generated(
        generated,
        "native-task-owned-assignment",
        "\n\n\n\nowned-assignment-owners-ok\n",
    );
}

const OWNED_ASSIGNMENT_MAIN: &str = r#"
typedef struct AssignmentRuntime { KuTaskDriverV1 driver; KuTaskDriverSlotV1 slots[4]; size_t ring[4]; } AssignmentRuntime;
static uint64_t assignment_deadline(void) { return ku_task_driver_now_ms()+2000u; }
static void assignment_track(unsigned index,void* pointer) {
  CHECK(index<5u); fixture_alloc_lock(); assignment_pointers[index]=(uintptr_t)pointer; fixture_alloc_unlock();
}
static size_t assignment_freed(unsigned index) {
  CHECK(index<5u); fixture_alloc_lock(); size_t value=assignment_frees[index]; fixture_alloc_unlock(); return value;
}
static void assignment_free(void* pointer) {
  fixture_alloc_lock();
  for (unsigned i=0;i<5u;i++) if (pointer&&(uintptr_t)pointer==assignment_pointers[i]) assignment_frees[i]++;
  fixture_alloc_unlock(); fixture_free(pointer);
}
static size_t assignment_write(const void* bytes,size_t size,size_t count,FILE* stream) {
  (void)stream;
  CHECK(size==1u&&count==3u&&!memcmp(bytes,"rhs",3u)&&!assignment_gate_calls++);
  assignment_track(4u,assignment_child);
  CHECK(ku_test_event_set(&assignment_entered)); CHECK(ku_test_event_wait(&assignment_proceed,2000));
  return count;
}
static KuTaskDriverSnapshotV1 assignment_snapshot(AssignmentRuntime* runtime) {
  KuTaskDriverSnapshotV1 value={0}; CHECK(ku_task_driver_snapshot(&runtime->driver,&value)==KU_TASK_DRIVER_OK); return value;
}
static void assignment_idle(AssignmentRuntime* runtime) {
  CHECK(ku_task_driver_wait_idle(&runtime->driver,assignment_deadline())==KU_TASK_DRIVER_OK);
}
static void assignment_init(AssignmentRuntime* runtime) {
  CHECK(!fixture_ledger().allocations&&!fixture_ledger().bytes);
  memset(runtime,0,sizeof(*runtime)); assignment_parent=NULL; assignment_child=NULL; assignment_gate_calls=0;
  memset(assignment_pointers,0,sizeof(assignment_pointers)); memset(assignment_frees,0,sizeof(assignment_frees));
  CHECK(ku_test_event_init(&assignment_entered)); CHECK(ku_test_event_init(&assignment_proceed));
  size_t fixed=sizeof(runtime->driver)+sizeof(runtime->slots)+sizeof(runtime->ring);
  CHECK(ku_task_driver_init(&runtime->driver,sizeof(runtime->driver),KU_TASK_DRIVER_ABI_VERSION,
      runtime->slots,4u,runtime->ring,4u,fixed+sizeof(KuTaskInstance_0)+sizeof(KuTaskInstance_1)
      +sizeof(KuTaskInstance_2)+sizeof(KuTaskInstance_3)+4096u,1u,assignment_deadline())==KU_TASK_DRIVER_OK);
}
static void assignment_finish(AssignmentRuntime* runtime) {
  assignment_idle(runtime); KuTaskDriverSnapshotV1 snapshot=assignment_snapshot(runtime);
  CHECK(!snapshot.resident&&!snapshot.reserved_bytes&&!snapshot.fault);
  uint64_t deadline=runtime->driver.closing ? runtime->driver.shutdown_deadline : assignment_deadline();
  CHECK(ku_task_driver_shutdown(&runtime->driver,deadline)==KU_TASK_DRIVER_OK);
  uint64_t effective=runtime->driver.shutdown_deadline; snapshot=assignment_snapshot(runtime);
  CHECK(snapshot.workers_exited==1u&&!snapshot.workers_joined&&!snapshot.running&&!snapshot.queued);
#if defined(_WIN32)
  if (ku_task_driver_now_ms()>=effective)
    CHECK(WaitForSingleObject(runtime->driver.workers[0].thread,2000)==WAIT_OBJECT_0);
#else
  alarm(2);
#endif
  CHECK(ku_task_driver_join(&runtime->driver,effective)==KU_TASK_DRIVER_OK);
  CHECK(runtime->driver.shutdown_deadline==effective&&runtime->driver.workers_joined==1u
      &&runtime->driver.workers[0].joined&&runtime->driver.workers[0].closed);
  CHECK(ku_task_driver_destroy(&runtime->driver)==KU_TASK_DRIVER_OK);
#if !defined(_WIN32)
  alarm(0);
#endif
  CHECK(ku_test_event_destroy(&assignment_entered)); CHECK(ku_test_event_destroy(&assignment_proceed));
  CHECK(!fixture_ledger().allocations&&!fixture_ledger().bytes&&!fixture_ledger().overflow);
}
static KuString assignment_string(unsigned index) {
  size_t capacity=index ? 37u : 31u,length=index ? 6u : 5u;
  uint8_t* pointer=(uint8_t*)malloc(capacity); CHECK(pointer); memcpy(pointer,index ? "second" : "first",length);
  assignment_track(index,pointer); return (KuString){pointer,length,capacity,KU_STRING_OWNED};
}
static void assignment_string_case(int64_t mode) {
  AssignmentRuntime runtime; assignment_init(&runtime);
  KuString old=assignment_string(0u),replacement=assignment_string(1u); KuTaskValueV1 task={0};
  CHECK(ku_task_1_start_value(&runtime.driver,&old,&replacement,&mode,&task)==KU_TASK_DRIVER_OK&&!old.ptr&&!replacement.ptr);
  CHECK(ku_test_event_wait(&assignment_entered,2000));
  KuTaskInstance_1* parent=(KuTaskInstance_1*)assignment_parent;
  KuTaskInstance_0* child=(KuTaskInstance_0*)assignment_child;
  CHECK(parent&&child&&parent->frame.header.status==KU_TASK_FRAME_PENDING&&assignment_gate_calls==1u);
  CHECK(!assignment_freed(0u)&&!assignment_freed(1u)&&!assignment_freed(4u));
  if (mode==2) {
    // An explicitly consumed RHS input is in the child, never resurrected in
    // the old destination merely because the replacement has not succeeded.
    CHECK(!(parent->frame.header.initialized&(UINT64_C(1)<<@VALUE@)));
    CHECK(!parent->frame.s_@VALUE@.ptr&&!parent->frame.s_@VALUE@.len&&!parent->frame.s_@VALUE@.capacity);
    CHECK((uintptr_t)child->frame.s_0.ptr==assignment_pointers[0]);
  } else {
    CHECK(parent->frame.header.initialized&(UINT64_C(1)<<@VALUE@));
    CHECK((uintptr_t)parent->frame.s_@VALUE@.ptr==assignment_pointers[0]);
    CHECK(parent->frame.s_@VALUE@.capacity==31u&&parent->frame.s_@VALUE@.len==5u);
    CHECK((uintptr_t)child->frame.s_0.ptr==assignment_pointers[1]);
  }
  size_t pending=sizeof(KuTaskInstance_0)+sizeof(KuTaskInstance_1)+31u+37u;
  CHECK(assignment_snapshot(&runtime).resident==2u&&assignment_snapshot(&runtime).reserved_bytes==pending);
  CHECK(fixture_ledger().bytes==pending);
  if (mode==3) {
    KuTaskControlLeaseV1 late={0}; CHECK(ku_task_control_lease_retain(&task.owner.lease,&late)==KU_TASK_CONTROL_OK);
    uint64_t deadline=ku_task_driver_now_ms()+30u; KuTaskDriverCleanupReceiptV1 receipt={0};
    CHECK(ku_task_driver_owner_drop_receipt(&task.ticket,&task.owner,deadline,&receipt)==KU_TASK_DRIVER_OK);
    task=(KuTaskValueV1){0}; CHECK(ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_PENDING);
    CHECK(ku_task_driver_shutdown(&runtime.driver,deadline)==KU_TASK_DRIVER_SHUTDOWN_TIMEOUT);
    CHECK(ku_task_control_cleanup_deadline(late.control)==deadline);
    CHECK(!assignment_freed(0u)&&!assignment_freed(1u)&&!assignment_freed(4u)&&fixture_ledger().bytes==pending);
    CHECK(ku_test_event_set(&assignment_proceed)); assignment_idle(&runtime);
    CHECK(ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_CLEANUP_ACK);
    CHECK(ku_task_control_cleanup_deadline(late.control)==deadline&&ku_task_control_status(&late)==KU_TASK_CONTROL_CANCELLED);
    CHECK(assignment_freed(0u)==1u&&assignment_freed(1u)==1u&&assignment_freed(4u)==1u);
    CHECK(assignment_snapshot(&runtime).resident==1u&&assignment_snapshot(&runtime).reserved_bytes==sizeof(KuTaskInstance_1));
    CHECK(fixture_ledger().bytes==sizeof(KuTaskInstance_1)&&late.control->frame_destroyed);
    CHECK(ku_task_control_lease_release(&late)==KU_TASK_CONTROL_OK); assignment_finish(&runtime); return;
  }
  CHECK(ku_test_event_set(&assignment_proceed));
  CHECK(ku_task_driver_wait_result(&task.ticket,&task.owner.lease,assignment_deadline())==KU_TASK_DRIVER_WAIT_READY);
  assignment_idle(&runtime);
  CHECK(assignment_freed(0u)==1u&&assignment_freed(1u)==(mode ? 1u : 0u)&&assignment_freed(4u)==1u);
  KuTaskDriverSnapshotV1 settled=assignment_snapshot(&runtime);
  size_t expected=sizeof(KuTaskInstance_1)+(mode ? 0u : 37u);
  if (settled.reserved_bytes!=expected || fixture_ledger().bytes!=expected)
    fprintf(stderr,"assignment mode=%lld reserved=%zu expected=%zu heap=%zu parent=%zu resident=%zu old_frees=%zu new_frees=%zu child_frees=%zu\n",
        (long long)mode,settled.reserved_bytes,expected,fixture_ledger().bytes,sizeof(KuTaskInstance_1),settled.resident,
        assignment_freed(0u),assignment_freed(1u),assignment_freed(4u));
  CHECK(settled.reserved_bytes==expected);
  CHECK(fixture_ledger().bytes==expected);
  size_t calls=fixture_ledger().calls; KuTaskAdapterOutcomeV1 output={0};
  CHECK(ku_task_value_take(&task,NULL,&output)==KU_TASK_CONTROL_OK&&output.result_kind==4u&&output.value.string.ok==(mode==0));
  CHECK(output.exit_class==KU_TASK_EXIT_USER_RESULT&&fixture_ledger().calls==calls);
  CHECK(assignment_snapshot(&runtime).reserved_bytes==sizeof(KuTaskInstance_1));
  if (!mode) {
    CHECK((uintptr_t)output.value.string.value.ptr==assignment_pointers[1]&&output.value.string.value.capacity==37u);
    CHECK(output.value.string.value.len==6u&&!memcmp(output.value.string.value.ptr,"second",6u));
  } else {
    CHECK(output.value.string.error.message.len==10u&&!memcmp(output.value.string.error.message.ptr,"rhs failed",10u));
  }
  ku_task_outcome_drop(&output); CHECK(assignment_freed(0u)==1u&&assignment_freed(1u)==1u);
  CHECK(ku_task_value_drop(&task,assignment_deadline())==KU_TASK_DRIVER_OK); assignment_finish(&runtime);
}
static KuArray_str assignment_array(unsigned replacement) {
  KuArray_str value={1u,NULL,replacement ? 2u : 4u};
  value.data=(KuString*)calloc(value.capacity,sizeof(*value.data)); CHECK(value.data);
  assignment_track(2u+replacement,value.data); value.data[0]=assignment_string(replacement); return value;
}
static void assignment_array_case(unsigned wrapped) {
  AssignmentRuntime runtime; assignment_init(&runtime);
  KuArray_str old=assignment_array(0u),replacement=assignment_array(1u); KuTaskValueV1 task={0};
  uint32_t started=wrapped ? ku_task_3_start_value(&runtime.driver,&old,&replacement,&task)
      : ku_task_2_start_value(&runtime.driver,&old,&replacement,&task);
  CHECK(started==KU_TASK_DRIVER_OK&&!old.data&&!replacement.data);
  CHECK(ku_task_driver_wait_result(&task.ticket,&task.owner.lease,assignment_deadline())==KU_TASK_DRIVER_WAIT_READY);
  assignment_idle(&runtime); size_t frame=wrapped ? sizeof(KuTaskInstance_3) : sizeof(KuTaskInstance_2);
  size_t active=2u*sizeof(KuString)+37u;
  CHECK(assignment_freed(0u)==1u&&assignment_freed(2u)==1u&&!assignment_freed(1u)&&!assignment_freed(3u));
  CHECK(assignment_snapshot(&runtime).resident==1u&&assignment_snapshot(&runtime).reserved_bytes==frame+active);
  CHECK(fixture_ledger().bytes==frame+active);
  size_t calls=fixture_ledger().calls; KuTaskAdapterOutcomeV1 output={0};
  CHECK(ku_task_value_take(&task,NULL,&output)==KU_TASK_CONTROL_OK&&output.result_kind==7u&&output.value.string_array.ok);
  KuArray_str value=output.value.string_array.value;
  CHECK((uintptr_t)value.data==assignment_pointers[3]&&value.len==1u&&value.capacity==2u);
  CHECK((uintptr_t)value.data[0].ptr==assignment_pointers[1]&&value.data[0].capacity==37u);
  CHECK(fixture_ledger().calls==calls&&assignment_snapshot(&runtime).reserved_bytes==frame);
  ku_task_outcome_drop(&output);
  CHECK(assignment_freed(0u)==1u&&assignment_freed(1u)==1u&&assignment_freed(2u)==1u&&assignment_freed(3u)==1u);
  CHECK(ku_task_value_drop(&task,assignment_deadline())==KU_TASK_DRIVER_OK); assignment_finish(&runtime);
}
static void assignment_raw_complete_case(void) {
  AssignmentRuntime runtime; assignment_init(&runtime);
  KuString old=assignment_string(0u),replacement=assignment_string(1u); KuTaskValueV1 task={0};
  CHECK(ku_task_5_start_value(&runtime.driver,&old,&replacement,&task)==KU_TASK_DRIVER_OK&&!old.ptr&&!replacement.ptr);
  CHECK(ku_task_driver_wait_result(&task.ticket,&task.owner.lease,assignment_deadline())==KU_TASK_DRIVER_WAIT_READY);
  assignment_idle(&runtime);
  CHECK(!assignment_gate_calls&&assignment_freed(0u)==1u&&!assignment_freed(1u));
  size_t expected=sizeof(KuTaskInstance_5)+37u; KuTaskDriverSnapshotV1 settled=assignment_snapshot(&runtime);
  if (settled.reserved_bytes!=expected||fixture_ledger().bytes!=expected)
    fprintf(stderr,"raw assignment reserved=%zu expected=%zu heap=%zu frame=%zu\n",
        settled.reserved_bytes,expected,fixture_ledger().bytes,sizeof(KuTaskInstance_5));
  CHECK(settled.resident==1u&&settled.reserved_bytes==expected&&fixture_ledger().bytes==expected);
  size_t calls=fixture_ledger().calls; KuTaskAdapterOutcomeV1 output={0};
  CHECK(ku_task_value_take(&task,NULL,&output)==KU_TASK_CONTROL_OK&&output.result_kind==4u&&output.value.string.ok);
  CHECK(output.exit_class==KU_TASK_EXIT_USER_RESULT&&fixture_ledger().calls==calls);
  CHECK((uintptr_t)output.value.string.value.ptr==assignment_pointers[1]&&output.value.string.value.capacity==37u);
  CHECK(output.value.string.value.len==6u&&!memcmp(output.value.string.value.ptr,"second",6u));
  CHECK(assignment_snapshot(&runtime).reserved_bytes==sizeof(KuTaskInstance_5));
  ku_task_outcome_drop(&output); CHECK(assignment_freed(0u)==1u&&assignment_freed(1u)==1u);
  CHECK(fixture_ledger().bytes==sizeof(KuTaskInstance_5));
  CHECK(ku_task_value_drop(&task,assignment_deadline())==KU_TASK_DRIVER_OK); assignment_finish(&runtime);
}
int main(void) {
  CHECK(KU_TASK_FRAME_ABI_VERSION==5u&&KU_TASK_CONTROL_ABI_VERSION==2u&&KU_TASK_DRIVER_ABI_VERSION==11u);
  assignment_raw_complete_case();
  for (int64_t mode=0;mode<4;mode++) assignment_string_case(mode);
  assignment_array_case(0u); assignment_array_case(1u);
  puts("owned-assignment-owners-ok"); return 0;
}
"#;

#[test]
fn native_task_string_views_clone_concat_and_pending_rhs_keep_exact_owners_and_fees() {
    let source = r#"
async fn Join(value: str, suffix: str): str! {
    copy=value.clone()
    joined=copy+suffix
    if (value.len()!=5 || value.byte_len()!=12 || copy.len()!=5 || suffix.len()!=1) { fail "views" }
    if (joined.len()!=6 || joined.byte_len()!=15) { fail "length" }
    return ok(joined)
}
async fn Empty(value: str): str! {
    copy=value.clone()
    if (value.len()!=0 || copy.byte_len()!=0) { fail "empty" }
    return ok(copy)
}
async fn Right(value: str, stop: bool): str! {
    println("rhs")
    if (stop) { fail "rhs failed" }
    return ok(value)
}
async fn JoinAwait(value: str, suffix: str, stop: bool): str! {
    joined=value+(await Right(suffix,stop))?
    if (value.byte_len()!=12 || joined.len()!=6) { fail "pending view" }
    return ok(joined)
}
async fn main(): null! { return ok(null) }
"#;
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let mut native = ir::task_lower::lower_program(&ast).unwrap();
    let raw = TaskFunction {
        id: TaskFunctionId(5),
        name: "RawStringClone".into(),
        slots: vec![
            slot(IrType::Str),
            slot(IrType::Str),
            slot(result(IrType::Str)),
        ],
        parameters: vec![SlotId(0)],
        entry: StateId(0),
        result: result(IrType::Str),
        states: vec![TaskState {
            operations: vec![
                TaskOp::StringClone {
                    dst: SlotId(1),
                    src: SlotId(0),
                },
                TaskOp::WrapOk {
                    dst: SlotId(2),
                    src: SlotId(1),
                },
                TaskOp::Drop { slot: SlotId(0) },
            ],
            terminator: TaskTerminator::Complete { value: SlotId(2) },
        }],
    };
    assert_eq!(native.tasks.functions.len(), 5);
    native.tasks.functions.push(raw.clone());
    let source_slot = native.tasks.functions[3].parameters[0];
    // Exercise the plain frame entry independently of source Exit/child/array
    // admission. The same raw graph also executes below in native's real C.
    let mut plain_raw = raw;
    plain_raw.id = TaskFunctionId(0);
    let sync_ast = Parser::new(Lexer::new("fn main() {}").lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&sync_ast).unwrap();
    let sync = ir::lower_program(&sync_ast).unwrap();
    let plain = c::generate_task_frame_c_source(
        &sync,
        &TaskProgram {
            functions: vec![plain_raw],
        },
    )
    .unwrap();
    assert!(plain.contains("ku_task_0_reconcile_charge"));
    let hooks = r#"
static KuTestEvent assignment_entered,assignment_proceed;
static void *assignment_parent,*assignment_child,*string_runtime;
static uintptr_t assignment_pointers[5];
static size_t assignment_frees[5];
static unsigned assignment_gate_calls;
static void assignment_free(void*);
static size_t assignment_write(const void*,size_t,size_t,FILE*);
static void* string_allocate(size_t);
static void string_overflow_probe(void*);
#undef free
#define free assignment_free
#define fwrite assignment_write
"#;
    let generated = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
    let instrumentation = format!(
        "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{}\n{ALLOCATION_HOOK}\n{}\n{hooks}",
        native_task_ledger::LEDGER_LOCK,
        native_task_ledger::LOCKED_ALLOCATIONS
    );
    assert_eq!(generated.matches("typedef struct KuString {").count(), 1);
    let generated = generated.replacen(
        "typedef struct KuString {",
        &format!("{instrumentation}typedef struct KuString {{"),
        1,
    );
    assert_eq!(generated.matches("int main(void) {").count(), 1);
    let mut generated = generated.replacen(
        "int main(void) {",
        "static int fixture_unmodified_string_main(void) {",
        1,
    );
    let allocation = "uint8_t* bytes=(uint8_t*)malloc(capacity);";
    assert_eq!(
        generated.matches(allocation).count(),
        2,
        "clone and concat allocations"
    );
    generated = generated.replace(
        allocation,
        "uint8_t* bytes=(uint8_t*)string_allocate(capacity);",
    );
    let concat = "KuString left, KuString right, KuString* output) {";
    assert_eq!(generated.matches(concat).count(), 1);
    generated = generated.replacen(
        concat,
        &format!("{concat}\n  string_overflow_probe(host);"),
        1,
    );
    for (id, name) in [(2, "child"), (3, "parent")] {
        let resume = format!("static uint32_t ku_task_{id}_resume(void* raw) {{");
        assert_eq!(generated.matches(&resume).count(), 1);
        generated = generated.replacen(&resume, &format!("{resume}\n  assignment_{name}=raw;"), 1);
    }
    // Reuse the preceding fixture's real allocation/free observations, event
    // gate, original-D shutdown and independent OS-return-tail observation.
    let common = OWNED_ASSIGNMENT_MAIN
        .split_once("static KuString assignment_string(")
        .unwrap()
        .0;
    let budget = "fixed+sizeof(KuTaskInstance_0)+sizeof(KuTaskInstance_1)\n      +sizeof(KuTaskInstance_2)+sizeof(KuTaskInstance_3)+4096u";
    assert_eq!(common.matches(budget).count(), 1);
    generated.push_str(
        &common
            .replace(
                "static void assignment_init(AssignmentRuntime* runtime)",
                "static void assignment_init(AssignmentRuntime* runtime,size_t budget)",
            )
            .replace(budget, "fixed+budget"),
    );
    generated.push_str(&STRING_OPERATIONS_MAIN.replace("@SOURCE@", &source_slot.0.to_string()));
    run_finally_adapter_generated(
        generated,
        "native-task-string-operations",
        "\n\n\nstring-operation-owners-ok\n",
    );
}

const STRING_OPERATIONS_MAIN: &str = r#"
static const uint8_t string_value[12]={0xe4,0xb8,0xad,0xe6,0x96,0x87,0xf0,0x9f,0x99,0x82,0,0x41};
static const uint8_t string_suffix[3]={0xe5,0xb0,0xbe};
static size_t string_calls,string_fail_at,string_actual_peak,string_prepaid_peak;
static void* string_allocations[3];
static unsigned string_probe,string_probe_seen;
static void string_init(AssignmentRuntime* runtime,size_t budget) {
  assignment_init(runtime,budget); string_runtime=runtime;
  string_calls=0; string_fail_at=0; string_actual_peak=0; string_prepaid_peak=0;
  memset(string_allocations,0,sizeof(string_allocations)); string_probe=0; string_probe_seen=0;
}
static void* string_allocate(size_t capacity) {
  CHECK(string_runtime&&string_calls<3u); size_t index=string_calls++;
  KuTaskDriverSnapshotV1 snapshot=assignment_snapshot((AssignmentRuntime*)string_runtime);
  if (snapshot.reserved_bytes>string_prepaid_peak) string_prepaid_peak=snapshot.reserved_bytes;
  void* pointer=string_fail_at==index+1u ? NULL : fixture_malloc(capacity);
  string_allocations[index]=pointer;
  size_t bytes=fixture_ledger().bytes; if (bytes>string_actual_peak) string_actual_peak=bytes;
  CHECK(!pointer||((uintptr_t)pointer!=assignment_pointers[0]&&(uintptr_t)pointer!=assignment_pointers[1]));
  return pointer;
}
static void string_overflow_probe(void* raw) {
  if (!string_probe) return;
  string_probe=0; string_probe_seen++;
  KuTaskAdapterHostV1* host=(KuTaskAdapterHostV1*)raw;
  // Synthetic lengths exercise the pre-read addition guard only. No claim is
  // made that the two one-byte literals back a SIZE_MAX-byte source buffer.
  KuString left=ku_string_static((const uint8_t*)"x",SIZE_MAX),right=ku_string_static((const uint8_t*)"y",1u);
  KuString output=ku_string_static((const uint8_t*)"keep",4u),before=output;
  size_t calls=fixture_ledger().calls,reserved=assignment_snapshot((AssignmentRuntime*)string_runtime).reserved_bytes;
  CHECK(ku_task_host_concat_string(host,left,right,&output)==KU_TASK_DRIVER_LIMIT);
  CHECK(output.ptr==before.ptr&&output.len==before.len&&output.capacity==before.capacity&&output.storage==before.storage);
  CHECK(fixture_ledger().calls==calls&&assignment_snapshot((AssignmentRuntime*)string_runtime).reserved_bytes==reserved);
}
static KuString string_input(unsigned suffix) {
  size_t capacity=suffix ? 37u : 31u,length=suffix ? 3u : 12u;
  uint8_t* bytes=(uint8_t*)malloc(capacity); CHECK(bytes); memcpy(bytes,suffix ? string_suffix : string_value,length);
  assignment_track(suffix,bytes); return (KuString){bytes,length,capacity,KU_STRING_OWNED};
}
static void string_check_join(KuTaskAdapterOutcomeV1* output) {
  CHECK(output->result_kind==4u&&output->value.string.ok&&output->exit_class==KU_TASK_EXIT_USER_RESULT);
  KuString value=output->value.string.value;
  CHECK(value.storage==KU_STRING_OWNED&&value.len==15u&&value.capacity==15u);
  CHECK(!memcmp(value.ptr,string_value,12u)&&!memcmp(value.ptr+12u,string_suffix,3u));
}
static void string_join_case(unsigned failure) {
  AssignmentRuntime runtime; size_t frame=sizeof(KuTaskInstance_0);
  string_init(&runtime,frame+(failure==1u ? 106u : 4096u));
  string_fail_at=failure==2u ? 3u : 0u; string_probe=failure==0u;
  KuString value=string_input(0u),suffix=string_input(1u); KuTaskValueV1 task={0};
  CHECK(ku_task_0_start_value(&runtime.driver,&value,&suffix,&task)==KU_TASK_DRIVER_OK&&!value.ptr&&!suffix.ptr);
  CHECK(ku_task_driver_wait_result(&task.ticket,&task.owner.lease,assignment_deadline())==KU_TASK_DRIVER_WAIT_READY);
  assignment_idle(&runtime);
  CHECK(assignment_freed(0u)==1u&&assignment_freed(1u)==1u);
  size_t active=failure ? 0u : 15u;
  CHECK(assignment_snapshot(&runtime).reserved_bytes==frame+active&&fixture_ledger().bytes==frame+active);
  if (string_calls!=(failure==1u ? 2u : 3u)||string_actual_peak!=frame+(failure ? 92u : 107u)
      ||string_prepaid_peak!=frame+(failure==1u ? 92u : 107u))
    fprintf(stderr,"string failure=%u calls=%zu actual_peak=%zu prepaid_peak=%zu frame=%zu reserved=%zu heap=%zu\n",
        failure,string_calls,string_actual_peak,string_prepaid_peak,frame,assignment_snapshot(&runtime).reserved_bytes,fixture_ledger().bytes);
  CHECK(string_calls==(failure==1u ? 2u : 3u));
  CHECK(string_actual_peak==frame+(failure ? 92u : 107u));
  CHECK(string_prepaid_peak==frame+(failure==1u ? 92u : 107u));
  CHECK(string_probe_seen==(failure==0u ? 1u : 0u));
  KuTaskAdapterOutcomeV1 output={0}; CHECK(ku_task_value_take(&task,NULL,&output)==KU_TASK_CONTROL_OK);
  CHECK(assignment_snapshot(&runtime).reserved_bytes==frame);
  if (!failure) {
    string_check_join(&output); CHECK(output.value.string.value.ptr==string_allocations[2]);
  } else {
    CHECK(!output.value.string.ok&&output.exit_class==KU_TASK_EXIT_RUNTIME_FAILURE);
    const char* expected=failure==1u ? "memory_limit" : "out_of_memory";
    CHECK(output.value.string.error.code.len==strlen(expected)&&!memcmp(output.value.string.error.code.ptr,expected,strlen(expected)));
  }
  ku_task_outcome_drop(&output); CHECK(fixture_ledger().bytes==frame);
  CHECK(ku_task_value_drop(&task,assignment_deadline())==KU_TASK_DRIVER_OK); assignment_finish(&runtime);
}
static void string_clone_case(unsigned empty) {
  AssignmentRuntime runtime; size_t frame=empty ? sizeof(KuTaskInstance_1) : sizeof(KuTaskInstance_5);
  size_t input_charge=empty ? 1u : 31u,output_charge=empty ? 1u : 12u;
  string_init(&runtime,frame+input_charge+output_charge);
  KuString value;
  if (empty) {
    uint8_t* bytes=(uint8_t*)malloc(1u); CHECK(bytes); bytes[0]=0;
    assignment_track(0u,bytes); value=(KuString){bytes,0u,0u,KU_STRING_OWNED};
  } else value=string_input(0u);
  KuTaskValueV1 task={0}; uint32_t status=empty ? ku_task_1_start_value(&runtime.driver,&value,&task)
      : ku_task_5_start_value(&runtime.driver,&value,&task);
  CHECK(status==KU_TASK_DRIVER_OK&&!value.ptr);
  CHECK(ku_task_driver_wait_result(&task.ticket,&task.owner.lease,assignment_deadline())==KU_TASK_DRIVER_WAIT_READY);
  assignment_idle(&runtime); CHECK(string_calls==1u&&assignment_freed(0u)==1u);
  CHECK(string_actual_peak==frame+input_charge+output_charge&&string_prepaid_peak==string_actual_peak);
  CHECK(assignment_snapshot(&runtime).reserved_bytes==frame+output_charge&&fixture_ledger().bytes==frame+output_charge);
  KuTaskAdapterOutcomeV1 output={0}; CHECK(ku_task_value_take(&task,NULL,&output)==KU_TASK_CONTROL_OK&&output.value.string.ok);
  KuString copied=output.value.string.value;
  CHECK(copied.ptr==string_allocations[0]&&(uintptr_t)copied.ptr!=assignment_pointers[0]);
  CHECK(copied.storage==KU_STRING_OWNED&&copied.capacity==output_charge&&copied.len==(empty ? 0u : 12u));
  CHECK(empty||!memcmp(copied.ptr,string_value,12u));
  ku_task_outcome_drop(&output); CHECK(fixture_ledger().bytes==frame);
  CHECK(ku_task_value_drop(&task,assignment_deadline())==KU_TASK_DRIVER_OK); assignment_finish(&runtime);
}
static void string_pending_case(unsigned mode) {
  AssignmentRuntime runtime; string_init(&runtime,sizeof(KuTaskInstance_2)+sizeof(KuTaskInstance_3)+4096u);
  KuString value=string_input(0u),suffix=string_input(1u); bool stop=mode==1u; KuTaskValueV1 task={0};
  CHECK(ku_task_3_start_value(&runtime.driver,&value,&suffix,&stop,&task)==KU_TASK_DRIVER_OK&&!value.ptr&&!suffix.ptr);
  CHECK(ku_test_event_wait(&assignment_entered,2000));
  KuTaskInstance_3* parent=(KuTaskInstance_3*)assignment_parent;
  CHECK(parent&&parent->frame.header.status==KU_TASK_FRAME_PENDING&&string_calls==1u&&string_allocations[0]);
  CHECK(!assignment_freed(0u)&&!assignment_freed(1u)&&!assignment_freed(4u));
  CHECK((uintptr_t)parent->frame.s_@SOURCE@.ptr==assignment_pointers[0]&&!memcmp(parent->frame.s_@SOURCE@.ptr,string_value,12u));
  CHECK(string_allocations[0]!=parent->frame.s_@SOURCE@.ptr&&!memcmp(string_allocations[0],string_value,12u));
  size_t pending=sizeof(KuTaskInstance_2)+sizeof(KuTaskInstance_3)+31u+37u+12u;
  CHECK(assignment_snapshot(&runtime).reserved_bytes==pending&&fixture_ledger().bytes==pending);
  if (mode==2u) {
    KuTaskControlLeaseV1 late={0}; CHECK(ku_task_control_lease_retain(&task.owner.lease,&late)==KU_TASK_CONTROL_OK);
    uint64_t deadline=ku_task_driver_now_ms()+30u; KuTaskDriverCleanupReceiptV1 receipt={0};
    CHECK(ku_task_driver_owner_drop_receipt(&task.ticket,&task.owner,deadline,&receipt)==KU_TASK_DRIVER_OK); task=(KuTaskValueV1){0};
    CHECK(ku_task_driver_shutdown(&runtime.driver,deadline)==KU_TASK_DRIVER_SHUTDOWN_TIMEOUT);
    CHECK(ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_PENDING&&ku_task_control_cleanup_deadline(late.control)==deadline);
    CHECK(fixture_ledger().bytes==pending&&!assignment_freed(0u)&&!assignment_freed(1u));
    CHECK(ku_test_event_set(&assignment_proceed)); assignment_idle(&runtime);
    CHECK(ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_CLEANUP_ACK&&ku_task_control_cleanup_deadline(late.control)==deadline);
    CHECK(assignment_freed(0u)==1u&&assignment_freed(1u)==1u&&assignment_freed(4u)==1u);
    CHECK(assignment_snapshot(&runtime).resident==1u&&assignment_snapshot(&runtime).reserved_bytes==sizeof(KuTaskInstance_3));
    CHECK(fixture_ledger().bytes==sizeof(KuTaskInstance_3));
    CHECK(ku_task_control_lease_release(&late)==KU_TASK_CONTROL_OK); assignment_finish(&runtime); return;
  }
  CHECK(ku_test_event_set(&assignment_proceed));
  CHECK(ku_task_driver_wait_result(&task.ticket,&task.owner.lease,assignment_deadline())==KU_TASK_DRIVER_WAIT_READY);
  assignment_idle(&runtime); CHECK(assignment_freed(0u)==1u&&assignment_freed(1u)==1u&&assignment_freed(4u)==1u);
  CHECK(string_calls==(mode ? 1u : 2u));
  CHECK(assignment_snapshot(&runtime).reserved_bytes==sizeof(KuTaskInstance_3)+(mode ? 0u : 15u));
  KuTaskAdapterOutcomeV1 output={0}; CHECK(ku_task_value_take(&task,NULL,&output)==KU_TASK_CONTROL_OK);
  if (!mode) string_check_join(&output);
  else CHECK(!output.value.string.ok&&output.value.string.error.message.len==10u&&!memcmp(output.value.string.error.message.ptr,"rhs failed",10u));
  ku_task_outcome_drop(&output); CHECK(fixture_ledger().bytes==sizeof(KuTaskInstance_3));
  CHECK(ku_task_value_drop(&task,assignment_deadline())==KU_TASK_DRIVER_OK); assignment_finish(&runtime);
}
int main(void) {
  CHECK(KU_TASK_FRAME_ABI_VERSION==5u&&KU_TASK_CONTROL_ABI_VERSION==2u&&KU_TASK_DRIVER_ABI_VERSION==11u);
  string_clone_case(0u); string_clone_case(1u);
  for (unsigned failure=0;failure<3u;failure++) string_join_case(failure);
  for (unsigned mode=0;mode<3u;mode++) string_pending_case(mode);
  puts("string-operation-owners-ok"); return 0;
}
"#;

#[test]
fn native_task_array_try_get_preserves_views_and_cleans_real_owned_results() {
    let source = r#"
async fn Pick(values: [str], index: int): str! {
    selected=values.try_get(index)
    if (values.len()!=2) { fail "source consumed" }
    return selected
}
async fn PickInt(values: [int], index: int): int! {
    selected=values.try_get(index)
    if (values.len()!=2) { fail "source consumed" }
    return selected
}
async fn PickBool(values: [bool], index: int): bool! {
    selected=values.try_get(index)
    if (values.len()!=2) { fail "source consumed" }
    return selected
}
async fn Index(values: [str], stop: bool): int! {
    println("rhs")
    if (stop) { fail "getter index failed" }
    return ok(values.len()-1)
}
async fn Pending(values: [str], stop: bool): str! {
    try { return ok(values.try_get((await Index(values,stop))?)?) }
    finally { println("done") }
    fail "unreachable"
}
async fn main(): null! { return ok(null) }
"#;
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let native = ir::task_lower::lower_program(&ast).unwrap();
    assert_eq!(native.tasks.functions.len(), 6);
    for function in &native.tasks.functions[..3] {
        assert_eq!(
            function
                .states
                .iter()
                .flat_map(|state| &state.operations)
                .filter(|op| matches!(op, TaskOp::ArrayTryGet { .. }))
                .count(),
            1
        );
        assert!(function
            .states
            .iter()
            .flat_map(|state| &state.operations)
            .all(|op| !matches!(op, TaskOp::ArrayClone { .. })));
    }
    let parent = &native.tasks.functions[4];
    let snapshots: Vec<_> = parent
        .states
        .iter()
        .flat_map(|state| &state.operations)
        .filter_map(|op| match op {
            TaskOp::ArrayClone { dst, src } if *src == parent.parameters[0] => Some(*dst),
            _ => None,
        })
        .collect();
    assert_eq!(snapshots.len(), 1);
    let hooks = r#"
static KuTestEvent assignment_entered,assignment_proceed;
static void *assignment_parent,*assignment_child,*getter_runtime;
static uintptr_t assignment_pointers[5];
static size_t assignment_frees[5];
static unsigned assignment_gate_calls,getter_finally_calls;
static void assignment_free(void*);
static size_t assignment_write(const void*,size_t,size_t,FILE*);
static size_t getter_write(const void*,size_t,size_t,FILE*);
static void* getter_allocate(size_t);
#undef free
#define free assignment_free
#define fwrite getter_write
"#;
    let generated = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
    let instrumentation = format!(
        "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{}\n{ALLOCATION_HOOK}\n{}\n{hooks}",
        native_task_ledger::LEDGER_LOCK,
        native_task_ledger::LOCKED_ALLOCATIONS
    );
    assert_eq!(generated.matches("typedef struct KuString {").count(), 1);
    let generated = generated.replacen(
        "typedef struct KuString {",
        &format!("{instrumentation}typedef struct KuString {{"),
        1,
    );
    assert_eq!(generated.matches("int main(void) {").count(), 1);
    let mut generated = generated.replacen(
        "int main(void) {",
        "static int fixture_unmodified_getter_main(void) {",
        1,
    );
    // Reuse the existing finally fixture's logical-clock wrapper. Only the
    // within-D cancellation case freezes time; the process watchdog stays real.
    let now = "static uint64_t ku_task_driver_now_ms(void) {";
    assert_eq!(generated.matches(now).count(), 1);
    generated = generated.replacen(
        now,
        "static uint64_t ku_task_driver_now_ms(void);\nstatic uint64_t getter_real_now(void) {",
        1,
    );
    let allocation = "uint8_t* bytes=(uint8_t*)malloc(capacity);";
    assert_eq!(generated.matches(allocation).count(), 2);
    generated = generated.replace(
        allocation,
        "uint8_t* bytes=(uint8_t*)getter_allocate(capacity);",
    );
    for (id, name) in [(3, "child"), (4, "parent")] {
        let resume = format!("static uint32_t ku_task_{id}_resume(void* raw) {{");
        assert_eq!(generated.matches(&resume).count(), 1);
        generated = generated.replacen(&resume, &format!("{resume}\n  assignment_{name}=raw;"), 1);
    }
    // Keep the existing ledger, exact original-D reaping and thread/event
    // protocol. Only this fixture receives its own input-dependent budget.
    let common = OWNED_ASSIGNMENT_MAIN
        .split_once("static KuString assignment_string(")
        .unwrap()
        .0;
    let budget = "fixed+sizeof(KuTaskInstance_0)+sizeof(KuTaskInstance_1)\n      +sizeof(KuTaskInstance_2)+sizeof(KuTaskInstance_3)+4096u";
    assert_eq!(common.matches(budget).count(), 1);
    generated.push_str(
        &common
            .replace(
                "static void assignment_init(AssignmentRuntime* runtime)",
                "static void assignment_init(AssignmentRuntime* runtime,size_t budget)",
            )
            .replace(budget, "fixed+budget"),
    );
    generated.push_str(&ARRAY_TRY_GET_MAIN.replace("@SNAPSHOT@", &snapshots[0].0.to_string()));
    run_finally_adapter_generated(
        generated,
        "native-task-array-try-get",
        "\n\n\n\n\n\n\narray-try-get-owners-ok\n",
    );
}

const ARRAY_TRY_GET_MAIN: &str = r#"
static const uint8_t getter_text[12]={0xe4,0xb8,0xad,0xe6,0x96,0x87,0xf0,0x9f,0x99,0x82,0,0x41};
static size_t getter_calls,getter_fail_at,getter_peak;
static void* getter_allocations[4];
static KuAtomicRefcount getter_clock_frozen;
static uint64_t getter_frozen_now;
static uint64_t ku_task_driver_now_ms(void) {
  return ku_task_control_atomic_load(&getter_clock_frozen) ? getter_frozen_now : getter_real_now();
}
static size_t getter_write(const void* bytes,size_t size,size_t count,FILE* stream) {
  if (size==1u&&count==4u&&!memcmp(bytes,"done",4u)) { getter_finally_calls++; return count; }
  return assignment_write(bytes,size,count,stream);
}
static void getter_init(AssignmentRuntime* runtime,size_t budget) {
  ku_task_control_atomic_store(&getter_clock_frozen,0u);
  assignment_init(runtime,budget); getter_runtime=runtime; getter_calls=0; getter_fail_at=0; getter_peak=0;
  getter_finally_calls=0; memset(getter_allocations,0,sizeof(getter_allocations));
}
static void* getter_allocate(size_t capacity) {
  CHECK(getter_calls<4u); size_t index=getter_calls++;
  KuTaskDriverSnapshotV1 snapshot=assignment_snapshot((AssignmentRuntime*)getter_runtime);
  if (snapshot.reserved_bytes>getter_peak) getter_peak=snapshot.reserved_bytes;
  void* pointer=getter_fail_at==index+1u ? NULL : fixture_malloc(capacity);
  getter_allocations[index]=pointer; return pointer;
}
static KuArray_str getter_input(unsigned immutable) {
  KuString* data=(KuString*)malloc(3u*sizeof(KuString)); CHECK(data); memset(data,0,3u*sizeof(KuString));
  assignment_track(2u,data);
  if (immutable) {
    data[0]=ku_string_static(getter_text,sizeof(getter_text)); data[1]=ku_string_static((const uint8_t*)"",0);
  } else {
    uint8_t* text=(uint8_t*)malloc(31u); CHECK(text); memcpy(text,getter_text,sizeof(getter_text));
    uint8_t* empty=(uint8_t*)malloc(1u); CHECK(empty); empty[0]=0;
    assignment_track(0u,text); assignment_track(1u,empty);
    data[0]=(KuString){text,12u,31u,KU_STRING_OWNED};
    // A real one-byte allocation with the canonical zero-capacity owned header.
    data[1]=(KuString){empty,0u,0u,KU_STRING_OWNED};
  }
  return (KuArray_str){.len=2u,.data=data,.capacity=3u};
}
static void getter_bounds(KuError error) {
  CHECK(error.domain.len==5u&&!memcmp(error.domain.ptr,"array",5u));
  CHECK(error.code.len==19u&&!memcmp(error.code.ptr,"index_out_of_bounds",19u));
  CHECK(error.message.len==25u&&!memcmp(error.message.ptr,"array index out of bounds",25u));
  CHECK(error.domain.storage==KU_STRING_STATIC&&error.code.storage==KU_STRING_STATIC&&error.message.storage==KU_STRING_STATIC);
}
static void getter_string_case(int64_t index,unsigned immutable,unsigned failure) {
  AssignmentRuntime runtime; size_t frame=sizeof(KuTaskInstance_0),input=3u*sizeof(KuString)+(immutable ? 0u : 32u);
  size_t copy=index==0 ? 12u : 1u;
  getter_init(&runtime,frame+input+(failure==1u ? copy-1u : 4096u));
  getter_fail_at=failure==2u ? 1u : 0u;
  KuArray_str values=getter_input(immutable); KuTaskValueV1 task={0};
  CHECK(ku_task_0_start_value(&runtime.driver,&values,&index,&task)==KU_TASK_DRIVER_OK&&!values.data);
  CHECK(ku_task_driver_wait_result(&task.ticket,&task.owner.lease,assignment_deadline())==KU_TASK_DRIVER_WAIT_READY);
  assignment_idle(&runtime);
  bool bounds=index<0||index>=2; size_t output_charge=(!bounds&&!failure&&!immutable) ? copy : 0u;
  CHECK(assignment_freed(2u)==1u&&assignment_freed(0u)==(immutable ? 0u : 1u)&&assignment_freed(1u)==(immutable ? 0u : 1u));
  CHECK(!getter_finally_calls&&!assignment_gate_calls);
  CHECK(getter_calls==((bounds||immutable||failure==1u) ? 0u : 1u));
  // Pure indices have no array/vector clone. Only the selected Owned leaf is
  // prepaid, so this peak includes the original vector and both source leaves.
  CHECK(getter_peak==(!getter_calls ? 0u : frame+input+copy));
  CHECK(assignment_snapshot(&runtime).reserved_bytes==frame+output_charge&&fixture_ledger().bytes==frame+output_charge);
  KuTaskAdapterOutcomeV1 output={0}; CHECK(ku_task_value_take(&task,NULL,&output)==KU_TASK_CONTROL_OK);
  CHECK(assignment_snapshot(&runtime).reserved_bytes==frame&&output.result_kind==4u);
  if (failure) {
    CHECK(!output.value.string.ok&&output.exit_class==KU_TASK_EXIT_RUNTIME_FAILURE);
    const char* code=failure==1u ? "memory_limit" : "out_of_memory";
    CHECK(output.value.string.error.code.len==strlen(code)&&!memcmp(output.value.string.error.code.ptr,code,strlen(code)));
  } else if (bounds) {
    CHECK(!output.value.string.ok&&output.exit_class==KU_TASK_EXIT_USER_RESULT); getter_bounds(output.value.string.error);
  } else {
    KuString selected=output.value.string.value;
    CHECK(output.value.string.ok&&selected.len==(index==0 ? 12u : 0u));
    CHECK(selected.storage==(immutable ? KU_STRING_STATIC : KU_STRING_OWNED));
    CHECK(selected.capacity==output_charge);
    CHECK(immutable||selected.ptr==getter_allocations[0]);
    CHECK(index!=0||!memcmp(selected.ptr,getter_text,sizeof(getter_text)));
  }
  ku_task_outcome_drop(&output); CHECK(fixture_ledger().bytes==frame);
  CHECK(ku_task_value_drop(&task,assignment_deadline())==KU_TASK_DRIVER_OK); assignment_finish(&runtime);
}
static void getter_primitive_case(unsigned boolean,int64_t index) {
  AssignmentRuntime runtime; size_t frame=boolean ? sizeof(KuTaskInstance_2) : sizeof(KuTaskInstance_1);
  getter_init(&runtime,frame+4096u); KuTaskValueV1 task={0};
  if (boolean) {
    bool* data=(bool*)malloc(4u*sizeof(bool)); CHECK(data); data[0]=true; data[1]=false; assignment_track(2u,data);
    KuArray_bool values={.len=2u,.data=data,.capacity=4u};
    CHECK(ku_task_2_start_value(&runtime.driver,&values,&index,&task)==KU_TASK_DRIVER_OK&&!values.data);
  } else {
    int64_t* data=(int64_t*)malloc(4u*sizeof(int64_t)); CHECK(data); data[0]=7; data[1]=9; assignment_track(2u,data);
    KuArray_int values={.len=2u,.data=data,.capacity=4u};
    CHECK(ku_task_1_start_value(&runtime.driver,&values,&index,&task)==KU_TASK_DRIVER_OK&&!values.data);
  }
  CHECK(ku_task_driver_wait_result(&task.ticket,&task.owner.lease,assignment_deadline())==KU_TASK_DRIVER_WAIT_READY);
  assignment_idle(&runtime); CHECK(!getter_calls&&assignment_freed(2u)==1u);
  CHECK(assignment_snapshot(&runtime).reserved_bytes==frame&&fixture_ledger().bytes==frame);
  KuTaskAdapterOutcomeV1 output={0}; CHECK(ku_task_value_take(&task,NULL,&output)==KU_TASK_CONTROL_OK);
  CHECK(output.exit_class==KU_TASK_EXIT_USER_RESULT);
  if (boolean) {
    CHECK(output.result_kind==2u);
    if (index<0||index>=2) { CHECK(!output.value.boolean.ok); getter_bounds(output.value.boolean.error); }
    else CHECK(output.value.boolean.ok&&output.value.boolean.value==(index==0));
  } else {
    CHECK(output.result_kind==1u);
    if (index<0||index>=2) { CHECK(!output.value.integer.ok); getter_bounds(output.value.integer.error); }
    else CHECK(output.value.integer.ok&&output.value.integer.value==(index==0 ? 7 : 9));
  }
  ku_task_outcome_drop(&output); CHECK(ku_task_value_drop(&task,assignment_deadline())==KU_TASK_DRIVER_OK); assignment_finish(&runtime);
}
static void getter_pending_case(unsigned mode) {
  AssignmentRuntime runtime; size_t frames=sizeof(KuTaskInstance_3)+sizeof(KuTaskInstance_4);
  getter_init(&runtime,frames+4096u); KuArray_str values=getter_input(0u); bool stop=mode==1u; KuTaskValueV1 task={0};
  CHECK(ku_task_4_start_value(&runtime.driver,&values,&stop,&task)==KU_TASK_DRIVER_OK&&!values.data);
  CHECK(ku_test_event_wait(&assignment_entered,2000));
  KuTaskInstance_4* parent=(KuTaskInstance_4*)assignment_parent;
  CHECK(parent&&parent->frame.header.status==KU_TASK_FRAME_PENDING&&getter_calls==2u&&!getter_finally_calls);
  KuArray_str snapshot=parent->frame.s_@SNAPSHOT@;
  CHECK(snapshot.len==2u&&snapshot.capacity==2u&&(uintptr_t)snapshot.data!=assignment_pointers[2]);
  CHECK(snapshot.data[0].ptr==getter_allocations[0]&&snapshot.data[1].ptr==getter_allocations[1]);
  CHECK(!memcmp(snapshot.data[0].ptr,getter_text,sizeof(getter_text))&&snapshot.data[1].capacity==1u);
  CHECK(!assignment_freed(0u)&&!assignment_freed(1u)&&!assignment_freed(2u)&&!assignment_freed(4u));
  size_t pending=frames+5u*sizeof(KuString)+45u;
  CHECK(assignment_snapshot(&runtime).reserved_bytes==pending&&fixture_ledger().bytes==pending);
  if (mode==2u||mode==3u) {
    KuTaskControlLeaseV1 late={0}; CHECK(ku_task_control_lease_retain(&task.owner.lease,&late)==KU_TASK_CONTROL_OK);
    if (mode==3u) {
      getter_frozen_now=ku_task_driver_now_ms();
      ku_task_control_atomic_store(&getter_clock_frozen,1u);
    }
    uint64_t deadline=ku_task_driver_now_ms()+30u; KuTaskDriverCleanupReceiptV1 receipt={0};
    CHECK(ku_task_driver_owner_drop_receipt(&task.ticket,&task.owner,deadline,&receipt)==KU_TASK_DRIVER_OK); task=(KuTaskValueV1){0};
    // Late retirement deliberately exhausts the real original D. The other
    // case releases the real child while its unchanged logical D is still live.
    if (mode==2u) CHECK(ku_task_driver_shutdown(&runtime.driver,deadline)==KU_TASK_DRIVER_SHUTDOWN_TIMEOUT);
    CHECK(ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_PENDING&&ku_task_control_cleanup_deadline(late.control)==deadline);
    CHECK(fixture_ledger().bytes==pending&&!getter_finally_calls);
    CHECK(ku_test_event_set(&assignment_proceed)); assignment_idle(&runtime);
    CHECK(ku_task_driver_cleanup_receipt_read(&receipt)==KU_TASK_DRIVER_CLEANUP_ACK&&ku_task_control_cleanup_deadline(late.control)==deadline);
    CHECK(getter_calls==2u&&getter_finally_calls==(mode==3u ? 1u : 0u));
    CHECK(ku_task_control_status(&late)==KU_TASK_CONTROL_CANCELLED);
    CHECK(assignment_snapshot(&runtime).reserved_bytes==sizeof(KuTaskInstance_4)&&fixture_ledger().bytes==sizeof(KuTaskInstance_4));
    CHECK(assignment_freed(0u)==1u&&assignment_freed(1u)==1u&&assignment_freed(2u)==1u&&assignment_freed(4u)==1u);
    CHECK(ku_task_control_lease_release(&late)==KU_TASK_CONTROL_OK);
    if (mode==3u) {
      CHECK(ku_task_driver_now_ms()==getter_frozen_now&&getter_frozen_now<deadline);
      CHECK(ku_task_driver_shutdown(&runtime.driver,deadline)==KU_TASK_DRIVER_OK);
      CHECK(runtime.driver.shutdown_deadline==deadline);
      ku_task_control_atomic_store(&getter_clock_frozen,0u);
    }
    assignment_finish(&runtime); return;
  }
  CHECK(ku_test_event_set(&assignment_proceed));
  CHECK(ku_task_driver_wait_result(&task.ticket,&task.owner.lease,assignment_deadline())==KU_TASK_DRIVER_WAIT_READY);
  assignment_idle(&runtime);
  CHECK(getter_calls==(mode ? 2u : 3u)&&getter_finally_calls==1u);
  CHECK(assignment_freed(0u)==1u&&assignment_freed(1u)==1u&&assignment_freed(2u)==1u&&assignment_freed(4u)==1u);
  CHECK(assignment_snapshot(&runtime).reserved_bytes==sizeof(KuTaskInstance_4)+(mode ? 0u : 1u));
  CHECK(fixture_ledger().bytes==sizeof(KuTaskInstance_4)+(mode ? 0u : 1u));
  KuTaskAdapterOutcomeV1 output={0}; CHECK(ku_task_value_take(&task,NULL,&output)==KU_TASK_CONTROL_OK);
  if (mode) CHECK(!output.value.string.ok&&output.value.string.error.message.len==19u&&!memcmp(output.value.string.error.message.ptr,"getter index failed",19u));
  else CHECK(output.value.string.ok&&output.value.string.value.ptr==getter_allocations[2]&&!output.value.string.value.len&&output.value.string.value.capacity==1u);
  ku_task_outcome_drop(&output); CHECK(fixture_ledger().bytes==sizeof(KuTaskInstance_4));
  CHECK(ku_task_value_drop(&task,assignment_deadline())==KU_TASK_DRIVER_OK); assignment_finish(&runtime);
}
int main(void) {
  CHECK(KU_TASK_FRAME_ABI_VERSION==5u&&KU_TASK_CONTROL_ABI_VERSION==2u&&KU_TASK_DRIVER_ABI_VERSION==11u);
  getter_string_case(0,0u,0u); getter_string_case(1,0u,0u); getter_string_case(0,1u,0u);
  getter_string_case(-1,0u,0u); getter_string_case(2,0u,0u);
  getter_string_case(0,0u,1u); getter_string_case(0,0u,2u);
  getter_primitive_case(0u,1); getter_primitive_case(0u,-1);
  getter_primitive_case(1u,1); getter_primitive_case(1u,2);
  for (unsigned mode=0;mode<4u;mode++) getter_pending_case(mode);
  puts("array-try-get-owners-ok"); return 0;
}
"#;

#[test]
fn native_task_net_jobs_retain_exclusive_owners_until_real_completion() {
    let source = r#"
import net from "std.net"
import bytes from "std.bytes"
async fn Operation(write: bool): null! {
    client=net.client({host:"127.0.0.1",port:9,read_timeout_ms:50})?
    if (write) { payload=bytes.from_array([65,0,66])? client.write(payload)? }
    else { data=client.read(4096)? }
    client.close()
    return ok(null)
}
async fn main(): null! { return ok(null) }
"#;
    let mut ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    ast.items
        .retain(|item| !matches!(item, ku::ast::Item::Import(_)));
    for name in ["std:net", "std:bytes"] {
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
    let function = &native.tasks.functions[0];
    let client_slot = function
        .states
        .iter()
        .find_map(|state| match state.terminator {
            TaskTerminator::BlockingNet {
                operation: TaskNetOperation::Read,
                inputs,
                ..
            } => inputs[0],
            _ => None,
        })
        .expect("read owns the local client slot")
        .0;
    let generated = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
    let instrumentation = format!(
        "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{}\n{ALLOCATION_HOOK}\n{}\n{NET_OWNER_HOOKS}\n",
        native_task_ledger::LEDGER_LOCK,
        native_task_ledger::LOCKED_ALLOCATIONS
    );
    let mut generated = generated
        .replacen(
            "typedef struct KuString {",
            &format!("{instrumentation}typedef struct KuString {{"),
            1,
        )
        .replacen(
            "int main(void) {",
            "static int ku_generated_main(void) {",
            1,
        );
    let constructor="static KuResult_net_client ku_net_client_with_allocator(KuObject* config,\n    const KuNetAllocatorV1* allocator) {";
    assert_eq!(generated.matches(constructor).count(), 1);
    generated = generated.replacen(
        constructor,
        &format!("{constructor}\n{NET_OWNER_CONSTRUCTOR}"),
        1,
    );
    let read="static KuResult_bytes ku_net_read_with_allocator(KuNetClient* client,\n    int64_t requested, const KuNetAllocatorV1* allocator) {";
    assert_eq!(generated.matches(read).count(), 1);
    generated = generated.replacen(read, &format!("{read}\n{NET_OWNER_READ}"), 1);
    let write = "static KuResult_null ku_net_write(KuNetClient* client, KuBytes data) {";
    assert_eq!(generated.matches(write).count(), 1);
    generated = generated.replacen(write, &format!("{write}\n{NET_OWNER_WRITE}"), 1);
    let close = "static uint8_t ku_net_close(KuNetClient* client) {\n  if (!client) return 0;";
    assert_eq!(generated.matches(close).count(), 1);
    generated = generated.replacen(
        close,
        &format!("{close}\n  fixture_alloc_lock(); fixture_net_closes++; fixture_alloc_unlock();"),
        1,
    );
    let drained = "uint32_t status=ku_task_0_drain(instance,reason,budget);";
    assert_eq!(generated.matches(drained).count(), 1);
    generated=generated.replacen(drained,&format!("{drained}\n  if (instance->values_cleaned) CHECK(ku_test_event_set(&fixture_net_drained));"),1);
    generated.push_str(&NET_OWNER_MAIN.replace("@CLIENT@", &client_slot.to_string()));
    let directory = TempDir::new("native-task-net-job-owners");
    let path = directory.path().join("owners.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "owners") else {
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
        "net-job-owners-ok"
    );
    let layout = String::from_utf8_lossy(&output.stderr);
    assert!(layout.contains("network-job-layout job="), "{layout}");
    eprint!("{layout}");
}

const NET_OWNER_HOOKS: &str = r#"
static KuTestEvent fixture_net_entered,fixture_net_release,fixture_net_drained;
static size_t fixture_net_closes,fixture_net_calls,fixture_net_output_bytes;
static uint64_t fixture_net_observed_deadline;
static int fixture_net_mode=1;
"#;
const NET_OWNER_CONSTRUCTOR: &str = r#"
  if (fixture_net_mode) {
    KuNetClient* fake=(KuNetClient*)ku_net_allocate(allocator,sizeof(KuNetClient));
    if (!fake) return (KuResult_net_client){false,NULL,ku_net_error("out_of_memory","fixture client allocation failed")};
    memset(fake,0,sizeof(*fake)); fake->socket_value=KU_NET_INVALID_SOCKET;
    ku_net_atomic_flag_init(&fake->poison_requested);
    CHECK(ku_net_gate_init(&fake->gate)==0);
    fake->read_timeout_ms=50u; fake->write_timeout_ms=50u; fake->max_read_bytes=4096u;
    return (KuResult_net_client){true,fake,{0}};
  }
"#;
const NET_OWNER_READ: &str = r#"
  if (fixture_net_mode) {
    CHECK(client && requested==4096);
    KuBytes value={0}; value.ptr=(uint8_t*)ku_net_allocate(allocator,(size_t)requested);
    if (value.ptr) {value.len=3u;value.capacity=(size_t)requested;value.storage=KU_BYTES_OWNED;memcpy(value.ptr,"A\0B",3u);}
    fixture_alloc_lock();fixture_net_calls++;fixture_net_output_bytes=value.capacity;
    fixture_net_observed_deadline=__ku_handler_deadline;fixture_alloc_unlock();
    CHECK(ku_test_event_set(&fixture_net_entered));CHECK(ku_test_event_wait(&fixture_net_release,2000));
    if (!value.ptr) return (KuResult_bytes){false,{0},ku_net_error("out_of_memory","fixture output allocation failed")};
    return (KuResult_bytes){true,value,{0}};
  }
"#;
const NET_OWNER_WRITE: &str = r#"
  if (fixture_net_mode) {
    CHECK(client);
    fixture_alloc_lock();fixture_net_calls++;fixture_net_observed_deadline=__ku_handler_deadline;fixture_alloc_unlock();
    CHECK(ku_test_event_set(&fixture_net_entered));CHECK(ku_test_event_wait(&fixture_net_release,2000));
    CHECK(data.ptr && data.len==3u && data.ptr[0]==65u && data.ptr[1]==0u && data.ptr[2]==66u);
    return (KuResult_null){true,0,{0}};
  }
"#;
const NET_OWNER_MAIN: &str = r#"
static KuTaskDriverV1 net_driver;
static KuTaskDriverSlotV1 net_slots[2];static size_t net_ring[2];
static KuTaskBlockingPoolV1 net_pool;
static FixtureLedger net_work_ledger(void) {
  FixtureLedger ledger=fixture_ledger();
  if(net_pool.storage) {
    CHECK(ledger.allocations && ledger.bytes>=net_pool.storage_bytes);
    ledger.allocations--;ledger.bytes-=net_pool.storage_bytes;
  }
  return ledger;
}

static uint64_t net_deadline(void) {uint64_t now=ku_task_driver_now_ms();CHECK(now!=UINT64_MAX);return now+2000u;}
static KuTaskDriverSnapshotV1 net_snapshot(void) {
  KuTaskDriverSnapshotV1 snapshot={0};CHECK(ku_task_driver_snapshot(&net_driver,&snapshot)==KU_TASK_DRIVER_OK);return snapshot;
}
/* Reuse one physical record across all network kinds, with non-empty owned
 * payloads/errors. Calling a drop for an inactive Result variant would free
 * the wrong payload or read an error at the wrong offset. */
static void net_result_storage_reuse(void) {
  KuTaskBlockingJobV1 job={0};
  CHECK((void*)&job.result==(void*)&job.read_result);
  CHECK((void*)&job.result==(void*)&job.net_connect_result);
  CHECK((void*)&job.result==(void*)&job.net_read_result);
  size_t separate=sizeof(job.result)+sizeof(job.read_result)+sizeof(job.net_connect_result)+sizeof(job.net_read_result);
  size_t largest=sizeof(job.net_read_result);
  if(sizeof(job.read_result)>largest)largest=sizeof(job.read_result);
  if(sizeof(job.net_connect_result)>largest)largest=sizeof(job.net_connect_result);
  if(sizeof(job.result)>largest)largest=sizeof(job.result);
  fprintf(stderr,"network-job-layout job=%zu result_members_sum=%zu shared_result_max=%zu fixed_1024=%zu fixed_16384=%zu\n",
      sizeof(job),separate,largest,ku_task_blocking_fixed_bytes(1024u),ku_task_blocking_fixed_bytes(16384u));
  for(size_t round=0;round<3u;round++)for(uint32_t kind=5u;kind<=8u;kind++)for(int ok=0;ok<2;ok++) {
    uint64_t generation=job.generation+1u;memset(&job,0,sizeof(job));job.kind=kind;job.generation=generation;
    FixtureLedger before=fixture_ledger();
    if(!ok) {
      KuString domain=ku_string_concat(ku_string_static((const uint8_t*)"do",2u),ku_string_static((const uint8_t*)"main",4u));
      KuString code=ku_string_concat(ku_string_static((const uint8_t*)"er",2u),ku_string_static((const uint8_t*)"ror",3u));
      KuString message=ku_string_concat(ku_string_static((const uint8_t*)"owned",5u),ku_string_static((const uint8_t*)" error",6u));
      KuError error=ku_error_make(domain,code,message);
      if(kind==5u)job.net_connect_result=(KuResult_net_client){false,NULL,error};
      else if(kind==6u)job.net_read_result=(KuResult_bytes){false,{0},error};
      else job.result=(KuResult_null){false,0,error};
    } else if(kind==5u) {
      job.net_connect_result=ku_net_client_with_allocator(NULL,NULL);CHECK(job.net_connect_result.ok);
    } else if(kind==6u) {
      uint8_t* bytes=(uint8_t*)malloc(3u);CHECK(bytes);memcpy(bytes,"A\0B",3u);
      job.net_read_result=(KuResult_bytes){true,{bytes,3u,3u,KU_BYTES_OWNED},{0}};
    } else job.result=(KuResult_null){true,0,{0}};
    ku_task_blocking_net_drop_result(&job);
    FixtureLedger after=fixture_ledger();CHECK(before.allocations==after.allocations&&before.bytes==after.bytes&&!after.overflow);
    CHECK(job.kind==kind&&job.generation==generation);
  }
}
static void net_case(int write,int cancel,int limited) {
  memset(&net_driver,0,sizeof(net_driver));memset(net_slots,0,sizeof(net_slots));memset(net_ring,0,sizeof(net_ring));memset(&net_pool,0,sizeof(net_pool));
  fixture_net_calls=fixture_net_closes=fixture_net_output_bytes=0u;fixture_net_observed_deadline=0;
  CHECK(ku_test_event_init(&fixture_net_entered));CHECK(ku_test_event_init(&fixture_net_release));CHECK(ku_test_event_init(&fixture_net_drained));
  size_t fixed=sizeof(net_driver)+sizeof(net_slots)+sizeof(net_ring)+ku_task_blocking_fixed_bytes(2u);
  size_t dynamic=sizeof(KuTaskInstance_0)+(limited?512u:16384u);uint64_t deadline=net_deadline();
  CHECK(ku_task_driver_init(&net_driver,sizeof(net_driver),KU_TASK_DRIVER_ABI_VERSION,net_slots,2u,net_ring,2u,fixed+dynamic,1u,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_blocking_init(&net_pool,2u,1u,deadline)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_attach_blocking(&net_driver,&net_pool,sizeof(net_pool),ku_task_blocking_close,ku_task_blocking_ready,net_pool.storage,net_pool.storage_bytes)==KU_TASK_DRIVER_OK);
  bool writing=write!=0;KuTaskValueV1 task={0};
  CHECK(ku_task_0_start_value(&net_driver,&writing,&task)==KU_TASK_DRIVER_OK);
  CHECK(ku_test_event_wait(&fixture_net_entered,2000));
  CHECK(ku_task_driver_wait_idle(&net_driver,net_deadline())==KU_TASK_DRIVER_OK);
  KuTaskInstance_0* instance=(KuTaskInstance_0*)task.owner.lease.control;
  CHECK(instance->frame_initialized && instance->frame.s_@CLIENT@==NULL);
  CHECK(instance->frame.header.initialized & (UINT64_C(1)<<@CLIENT@));
  CHECK(instance->blocking.pool==&net_pool && instance->blocking.charged_bytes>=sizeof(KuNetClient));
  CHECK(!fixture_net_closes && fixture_net_calls==1u);
  CHECK(fixture_net_output_bytes==(write||limited?0u:4096u));
  CHECK(net_snapshot().reserved_bytes==net_work_ledger().bytes);
  CHECK(fixture_net_observed_deadline && fixture_net_observed_deadline<=net_deadline());
  if (cancel) {
    uint64_t original=net_deadline();
    CHECK(ku_task_driver_request_cancel(&task.ticket,&task.owner.lease,KU_TASK_CONTROL_CANCELLED,original)==KU_TASK_CONTROL_OK);
    CHECK(ku_test_event_wait(&fixture_net_drained,2000));
    CHECK(ku_task_driver_wait_idle(&net_driver,net_deadline())==KU_TASK_DRIVER_OK);
    /* Pending cancellation terminates the frame's Values, retaining initialized
     * terminal frame storage until the actual job retires. Only READY cleanup
     * destroys storage immediately; frame_initialized is not a Value-owner bit. */
    CHECK(instance->values_cleaned && instance->frame_initialized);
    CHECK(instance->frame.header.status==KU_TASK_FRAME_CANCELLED);
    CHECK(!instance->frame.header.initialized && instance->frame.s_@CLIENT@==NULL);
    CHECK(!fixture_net_closes && instance->blocking.pool==&net_pool);
    CHECK(ku_task_control_cleanup_deadline(task.owner.lease.control)==original);
    CHECK(net_snapshot().reserved_bytes==net_work_ledger().bytes);
  }
  CHECK(ku_test_event_set(&fixture_net_release));
  CHECK(ku_task_driver_wait_result(&task.ticket,&task.owner.lease,net_deadline())==KU_TASK_DRIVER_WAIT_READY);
  CHECK(ku_task_driver_wait_idle(&net_driver,net_deadline())==KU_TASK_DRIVER_OK);
  CHECK(fixture_net_closes==1u && !instance->blocking.pool);
  CHECK(net_snapshot().reserved_bytes==sizeof(KuTaskInstance_0));
  CHECK(net_work_ledger().bytes==sizeof(KuTaskInstance_0));
  KuTaskAdapterOutcomeV1 outcome={0};uint32_t taken=ku_task_value_take(&task,NULL,&outcome);
  if (cancel) CHECK(taken==KU_TASK_CONTROL_CANCELLED && ku_task_outcome_empty(&outcome,3u));
  else {
    CHECK(taken==KU_TASK_CONTROL_OK);
    if (limited) CHECK(!outcome.value.null_value.ok && outcome.value.null_value.error.code.len==12u && !memcmp(outcome.value.null_value.error.code.ptr,"memory_limit",12u));
    else CHECK(outcome.value.null_value.ok);
  }
  ku_task_outcome_drop(&outcome);CHECK(ku_task_value_drop(&task,net_deadline())==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_wait_idle(&net_driver,net_deadline())==KU_TASK_DRIVER_OK);
  CHECK(!net_snapshot().resident && !net_snapshot().reserved_bytes);
  CHECK(ku_task_driver_shutdown(&net_driver,net_deadline())==KU_TASK_DRIVER_OK);
  CHECK(ku_task_blocking_finish(&net_pool,net_deadline())==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_join(&net_driver,net_deadline())==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_destroy(&net_driver)==KU_TASK_DRIVER_OK);
  CHECK(ku_test_event_destroy(&fixture_net_entered));CHECK(ku_test_event_destroy(&fixture_net_release));CHECK(ku_test_event_destroy(&fixture_net_drained));
  CHECK(!net_work_ledger().allocations && !net_work_ledger().bytes && !net_work_ledger().overflow);
}
int main(void) {
  CHECK(KU_TASK_DRIVER_ABI_VERSION==11u);
  net_result_storage_reuse();
  net_case(0,0,0);net_case(1,0,0);net_case(0,0,1);net_case(0,1,0);net_case(1,1,0);
  puts("net-job-owners-ok");return 0;
}
"#;
