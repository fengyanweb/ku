//! Internal R3 admission/driver fixtures over generated, serial R1 frames.
//! Source async, an I/O poller and production concurrency benchmarks are separate gates.

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
    let frames = TaskProgram {
        functions: vec![TaskFunction {
            id: TaskFunctionId(0),
            name: "DriverOwnedFrame".into(),
            slots: [result.clone(), IrType::Str]
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
                    operations: vec![TaskOp::Drop { slot: SlotId(1) }],
                    terminator: TaskTerminator::Complete { value: SlotId(0) },
                },
                TaskState {
                    operations: vec![
                        TaskOp::Drop { slot: SlotId(1) },
                        TaskOp::Drop { slot: SlotId(0) },
                    ],
                    terminator: TaskTerminator::Terminate,
                },
            ],
        }],
    };
    c::generate_task_frame_c_source(&sync, &frames).unwrap()
}

#[test]
fn native_task_driver_admission_wake_retirement_and_shutdown_execute_in_c() {
    let generated = fixture_source();
    for forbidden in ["run_source", "const SOURCE", "task.spawn", "Task.new"] {
        assert!(!generated.contains(forbidden));
    }
    assert!(generated.contains("KuTaskDriverV1"));
    let wait_start = generated
        .find("static int ku_task_driver_wait(")
        .expect("generated driver wait helper");
    let wait_end = wait_start
        + generated[wait_start..]
            .find("static int ku_task_driver_wait_without_clock(")
            .expect("end of generated driver wait helper");
    let generated_wait = &generated[wait_start..wait_end];
    let instrumentation = format!(
        "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\n{LOCKED_ALLOCATIONS}\n"
    );
    let mut source = generated
        .replacen(
            "typedef struct KuString {",
            &format!("{instrumentation}typedef struct KuString {{"),
            1,
        )
        .replacen(
            "int main(void) {",
            "static int ku_generated_main(void) {",
            1,
        )
        .replacen(
            "static uint64_t ku_task_driver_now_ms(void) {",
            &format!("{LINUX_CLOCK_FAULT_HOOK}\nstatic uint64_t ku_task_driver_now_ms(void) {{"),
            1,
        )
        .replacen(
            "static void ku_task_driver_worker(KuTaskDriverV1* driver) {",
            "static void ku_task_driver_worker(KuTaskDriverV1* driver) {\n#if defined(__linux__)\nfixture_is_driver_worker = 1;\n#endif",
            1,
        )
        .replacen(
            "static int ku_generated_main(void) {",
            "#if defined(__linux__)\n#undef clock_gettime\n#endif\nstatic int ku_generated_main(void) {",
            1,
        );
    source.push_str(LINUX_DEADLINE_HEAD);
    source.push_str(generated_wait);
    source.push_str(LINUX_DEADLINE_TAIL);
    source.push_str(C_MAIN);
    let directory = TempDir::new("task-driver-v1");
    let c_file = directory.path().join("driver.c");
    fs::write(&c_file, source).expect("write generated driver fixture");
    let Some(executable) = compile_harness(directory.path(), &c_file, "driver") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute the native driver fixture"
        );
        return;
    };
    fs::remove_file(&c_file).expect("remove C source before running driver artifact");
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("native driver fixture must remain bounded");
    assert!(
        output.status.success(),
        "native driver fixture failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-driver-v1-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const LEDGER_LOCK: &str = r#"
/* Reuse the shared allocation ledger, but serialize EVERY instrumented access:
 * real driver/main/take threads can free and allocate at the same time. */
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

const LINUX_CLOCK_FAULT_HOOK: &str = r#"
#if defined(__linux__)
static _Thread_local int fixture_clock_failure = 0;
static _Thread_local int fixture_is_driver_worker = 0;
static KuAtomicRefcount fixture_worker_clock_failure;
static KuTestEvent* fixture_worker_clock_failed;
static int fixture_driver_clock_gettime(clockid_t clock, struct timespec* output) {
  if (fixture_clock_failure) { errno = EIO; return -1; }
  if (fixture_is_driver_worker && ku_task_control_atomic_load(&fixture_worker_clock_failure)) {
    if (!ku_test_event_set(fixture_worker_clock_failed)) abort();
    errno = EIO; return -1;
  }
  return clock_gettime(clock, output);
}
/* Limited to the generated driver, not the bounded test harness's real clock. */
#define clock_gettime fixture_driver_clock_gettime
#endif
"#;

const LINUX_DEADLINE_HEAD: &str = r#"
#if defined(__linux__)
/* Compile the generated wait helper VERBATIM a second time with private OS
 * substitutes. This isolates its conversion logic without any real timed wait. */
static uint64_t fixture_wait_now = 1000;
static unsigned fixture_wait_now_calls, fixture_wait_clock_calls;
static unsigned fixture_wait_timed_calls, fixture_wait_infinite_calls;
static struct timespec fixture_wait_observed;
static uint64_t fixture_wait_now_ms(void) { ++fixture_wait_now_calls; return fixture_wait_now; }
static int fixture_wait_clock_gettime(clockid_t clock, struct timespec* output) {
  (void)clock; ++fixture_wait_clock_calls;
  output->tv_sec = 5; output->tv_nsec = 0; return 0;
}
static int fixture_wait_cond_timedwait(pthread_cond_t* condition, pthread_mutex_t* mutex,
                                      const struct timespec* deadline) {
  (void)condition; (void)mutex;
  ++fixture_wait_timed_calls; fixture_wait_observed = *deadline; return ETIMEDOUT;
}
static int fixture_wait_cond_wait(pthread_cond_t* condition, pthread_mutex_t* mutex) {
  (void)condition; (void)mutex; ++fixture_wait_infinite_calls; return 0;
}
#define ku_task_driver_wait fixture_generated_deadline_wait
#define ku_task_driver_now_ms fixture_wait_now_ms
#define clock_gettime fixture_wait_clock_gettime
#define pthread_cond_timedwait fixture_wait_cond_timedwait
#define pthread_cond_wait fixture_wait_cond_wait
"#;

const LINUX_DEADLINE_TAIL: &str = r#"
#undef ku_task_driver_wait
#undef ku_task_driver_now_ms
#undef clock_gettime
#undef pthread_cond_timedwait
#undef pthread_cond_wait
#define DEADLINE_CHECK(c) do { if (!(c)) { fprintf(stderr, "Linux deadline check line %d\n", __LINE__); abort(); } } while (0)
static void fixture_wait_reset(uint64_t now) {
  fixture_wait_now = now; fixture_wait_now_calls = fixture_wait_clock_calls = 0;
  fixture_wait_timed_calls = fixture_wait_infinite_calls = 0;
  memset(&fixture_wait_observed, 0, sizeof(fixture_wait_observed));
}
static void fixture_linux_absolute_deadline(void) {
  KuTaskDriverV1 unused;
  memset(&unused, 0, sizeof(unused));
  fixture_wait_reset(1000);
  DEADLINE_CHECK(fixture_generated_deadline_wait(&unused, 1050) == 1);
  DEADLINE_CHECK(fixture_wait_now_calls == 1 && fixture_wait_timed_calls == 1);
  /* The old second-clock-plus-old-remaining calculation produced 5.05 sec.
   * Our original absolute deadline is 1.05 sec regardless of preemption. */
  DEADLINE_CHECK(fixture_wait_observed.tv_sec == 1 && fixture_wait_observed.tv_nsec == 50000000L);
  fixture_wait_reset(1000);
  DEADLINE_CHECK(fixture_generated_deadline_wait(&unused, UINT64_MAX - 1) == 1);
  DEADLINE_CHECK(fixture_wait_observed.tv_sec == 86401 && fixture_wait_observed.tv_nsec == 0);
  fixture_wait_reset(1000);
  DEADLINE_CHECK(fixture_generated_deadline_wait(&unused, 1000) == 1);
  DEADLINE_CHECK(!fixture_wait_timed_calls && !fixture_wait_infinite_calls);
  fixture_wait_reset(1000);
  DEADLINE_CHECK(fixture_generated_deadline_wait(&unused, UINT64_MAX) == 0);
  DEADLINE_CHECK(!fixture_wait_timed_calls && fixture_wait_infinite_calls == 1);
  fixture_wait_reset(UINT64_MAX);
  DEADLINE_CHECK(fixture_generated_deadline_wait(&unused, 1050) == -2);
  DEADLINE_CHECK(!fixture_wait_timed_calls && !fixture_wait_infinite_calls);
  fixture_wait_reset(UINT64_MAX);
  DEADLINE_CHECK(fixture_generated_deadline_wait(&unused, UINT64_MAX) == -2);
  DEADLINE_CHECK(!fixture_wait_timed_calls && !fixture_wait_infinite_calls);
}
#undef DEADLINE_CHECK
#else
static void fixture_linux_absolute_deadline(void) {}
#endif
"#;

const LOCKED_ALLOCATIONS: &str = r#"
#undef malloc
#undef calloc
#undef realloc
#undef free
static size_t fixture_fail_in = SIZE_MAX;
static int fixture_alloc_fails(void) {
  if (fixture_fail_in == SIZE_MAX) return 0;
  if (fixture_fail_in == 0) { fixture_fail_in = SIZE_MAX; return 1; }
  --fixture_fail_in;
  return 0;
}
static void fixture_fail_allocation(size_t count) {
  fixture_alloc_lock(); fixture_fail_in = count; fixture_alloc_unlock();
}
static void* fixture_malloc(size_t size) {
  fixture_alloc_lock();
  void* result = fixture_alloc_fails() ? NULL : ku_perf_malloc(size);
  fixture_alloc_unlock(); return result;
}
static void* fixture_calloc(size_t count, size_t size) {
  fixture_alloc_lock();
  void* result = fixture_alloc_fails() ? NULL : ku_perf_calloc(count, size);
  fixture_alloc_unlock(); return result;
}
static void* fixture_realloc(void* value, size_t size) {
  fixture_alloc_lock();
  void* result = fixture_alloc_fails() ? NULL : ku_perf_realloc(value, size);
  fixture_alloc_unlock(); return result;
}
static void fixture_free(void* value) {
  fixture_alloc_lock(); ku_perf_free(value); fixture_alloc_unlock();
}
typedef struct FixtureLedger { size_t allocations, bytes; int overflow; } FixtureLedger;
static FixtureLedger fixture_ledger(void) {
  fixture_alloc_lock();
  FixtureLedger value = { ku_perf_live_allocations, ku_perf_live_bytes, ku_perf_overflow };
  fixture_alloc_unlock(); return value;
}
#define malloc fixture_malloc
#define calloc fixture_calloc
#define realloc fixture_realloc
#define free fixture_free
"#;

const C_MAIN: &str = r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr, "driver check line %d: %s\n", __LINE__, #c); abort(); } } while (0)
#define DRIVER_ABI KU_TASK_DRIVER_ABI_VERSION
#define FRAME_ABI KU_TASK_FRAME_ABI_VERSION
#define CONTROL_ABI KU_TASK_CONTROL_ABI_VERSION
typedef struct FixtureDriver {
  KuTaskDriverV1* driver;
  KuTaskDriverSlotV1* slots;
  size_t* ring;
  size_t capacity;
  size_t fixed_bytes;
} FixtureDriver;
typedef struct FixtureWitness {
  KuTestEvent entered, proceed, disposed;
  KuAtomicRefcount resumes, cleanups, frame_drops, payload_drops, takes, disposes;
  uint64_t cleanup_deadline;
} FixtureWitness;
typedef struct FixtureTask {
  KuTaskControlV1 control;
  KuTaskFrame_0* frame;
  KuResult_str payload;
  KuTaskDriverTicketV1 ticket;
  FixtureWitness* witness;
  bool frame_initialized;
  bool payload_initialized;
  bool registered_wait;
  bool block_pending;
  bool block_take;
  bool early_take_notify;
  bool block_cleanup;
  bool block_dispose;
} FixtureTask;
typedef struct FixtureInputs { KuResult_str result; KuString cleanup; } FixtureInputs;
static uint64_t fixture_deadline(void) { return ku_task_driver_now_ms() + 2000; }
static size_t fixture_count(KuAtomicRefcount* count) { return ku_task_control_atomic_load(count); }
static void fixture_increment(KuAtomicRefcount* count) {
  /* Each witness counter has a single callback writer. Atomic loads also make
   * cross-thread inspection legal before final disposal has completed. */
  ku_task_control_atomic_store(count, fixture_count(count) + 1);
}
static void fixture_witness_init(FixtureWitness* witness) {
  memset(witness, 0, sizeof(*witness));
  CHECK(ku_test_event_init(&witness->entered));
  CHECK(ku_test_event_init(&witness->proceed));
  CHECK(ku_test_event_init(&witness->disposed));
  ku_task_control_atomic_init(&witness->resumes, 0);
  ku_task_control_atomic_init(&witness->cleanups, 0);
  ku_task_control_atomic_init(&witness->frame_drops, 0);
  ku_task_control_atomic_init(&witness->payload_drops, 0);
  ku_task_control_atomic_init(&witness->takes, 0);
  ku_task_control_atomic_init(&witness->disposes, 0);
}
static void fixture_witness_finish(FixtureWitness* witness) {
  CHECK(ku_test_event_destroy(&witness->entered));
  CHECK(ku_test_event_destroy(&witness->proceed));
  CHECK(ku_test_event_destroy(&witness->disposed));
}
static KuString fixture_owned(const char* text) {
  size_t length = strlen(text);
  uint8_t* data = (uint8_t*)malloc(length ? length : 1);
  CHECK(data);
  if (length) memcpy(data, text, length);
  return (KuString){ data, length, length ? length : 1, KU_STRING_OWNED };
}
static FixtureInputs fixture_inputs(void) {
  FixtureInputs inputs = {0};
  inputs.result.ok = true;
  inputs.result.value = fixture_owned("payload");
  inputs.cleanup = fixture_owned("cleanup");
  return inputs;
}
static size_t fixture_task_bytes(void) {
  return sizeof(FixtureTask) + sizeof(KuTaskFrame_0) + 7 + 7;
}
static int fixture_charge_string(size_t* bytes, KuString value) {
  if (value.storage != KU_STRING_OWNED) return 1;
  if (value.capacity > SIZE_MAX - *bytes) return 0;
  *bytes += value.capacity; return 1;
}
static int fixture_input_charge(FixtureInputs* inputs, size_t* bytes) {
  *bytes = sizeof(FixtureTask) + sizeof(KuTaskFrame_0);
  if (!fixture_charge_string(bytes, inputs->cleanup)) return 0;
  if (inputs->result.ok) return fixture_charge_string(bytes, inputs->result.value);
  return fixture_charge_string(bytes, inputs->result.error.domain)
      && fixture_charge_string(bytes, inputs->result.error.code)
      && fixture_charge_string(bytes, inputs->result.error.message);
}
static void fixture_inputs_drop(FixtureInputs* inputs) {
  ku_result_drop_str(&inputs->result);
  ku_string_drop(&inputs->cleanup);
}
static void fixture_retain(const KuTaskControlLeaseV1* source, KuTaskControlLeaseV1* output) {
  /* R2's one-shot retain may contend with the driver's one-time pin release.
   * Retry only that documented nonconsuming Pending, bounded by count/time. */
  uint64_t deadline = fixture_deadline();
  for (unsigned attempt = 0; attempt < 64; ++attempt) {
    uint32_t retained = ku_task_control_lease_retain(source, output);
    if (retained == KU_TASK_CONTROL_OK) return;
    CHECK(retained == KU_TASK_CONTROL_PENDING && ku_task_driver_now_ms() < deadline);
    ku_test_thread_yield();
  }
  CHECK(false);
}
static KuTaskDriverSnapshotV1 fixture_snapshot(FixtureDriver* runtime) {
  KuTaskDriverSnapshotV1 snapshot;
  memset(&snapshot, 0, sizeof(snapshot));
  CHECK(ku_task_driver_snapshot(runtime->driver, &snapshot) == KU_TASK_DRIVER_OK);
  CHECK(snapshot.queued <= runtime->capacity && snapshot.running <= 1);
  CHECK(snapshot.resident <= runtime->capacity && snapshot.building <= snapshot.resident);
  CHECK(!snapshot.fault);
  return snapshot;
}
static void fixture_idle(FixtureDriver* runtime) {
  CHECK(ku_task_driver_wait_idle(runtime->driver, fixture_deadline()) == KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot = fixture_snapshot(runtime);
  CHECK(!snapshot.queued && !snapshot.running && (snapshot.worker_waiting || snapshot.worker_exited));
}
static void fixture_driver_init(FixtureDriver* runtime, size_t capacity, size_t task_budget) {
  memset(runtime, 0, sizeof(*runtime));
  runtime->capacity = capacity;
  runtime->driver = (KuTaskDriverV1*)calloc(1, sizeof(*runtime->driver));
  runtime->slots = (KuTaskDriverSlotV1*)calloc(capacity, sizeof(*runtime->slots));
  runtime->ring = (size_t*)calloc(capacity, sizeof(*runtime->ring));
  CHECK(runtime->driver && runtime->slots && runtime->ring);
  runtime->fixed_bytes = sizeof(*runtime->driver) + capacity * (sizeof(*runtime->slots) + sizeof(*runtime->ring));
  CHECK(task_budget <= SIZE_MAX - runtime->fixed_bytes);
  CHECK(ku_task_driver_init(runtime->driver, sizeof(*runtime->driver), DRIVER_ABI,
      runtime->slots, capacity, runtime->ring, capacity, runtime->fixed_bytes + task_budget) == KU_TASK_DRIVER_OK);
  fixture_idle(runtime);
  KuTaskDriverSnapshotV1 snapshot = fixture_snapshot(runtime);
  CHECK(!snapshot.resident && snapshot.fixed_bytes == runtime->fixed_bytes);
}
static void fixture_driver_finish(FixtureDriver* runtime) {
  CHECK(ku_task_driver_shutdown(runtime->driver, fixture_deadline()) == KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot = fixture_snapshot(runtime);
  CHECK(!snapshot.resident && !snapshot.building && !snapshot.queued && !snapshot.running);
  CHECK(!snapshot.parked && !snapshot.retiring && !snapshot.terminal_held);
  CHECK(snapshot.worker_exited);
  /* Windows may publish worker_exited just before its OS thread handle becomes
   * signaled. Retry only this final OS acknowledgement, bounded by count/time;
   * never poll a live callback or disguise failed task cleanup as a join. */
  uint64_t deadline = fixture_deadline();
  uint32_t destroyed = KU_TASK_DRIVER_PENDING;
  for (unsigned attempt = 0; attempt < 4096 && destroyed == KU_TASK_DRIVER_PENDING; ++attempt) {
    destroyed = ku_task_driver_destroy(runtime->driver);
    if (destroyed == KU_TASK_DRIVER_PENDING) {
      CHECK(ku_task_driver_now_ms() < deadline);
      ku_test_thread_yield();
    }
  }
  CHECK(destroyed == KU_TASK_DRIVER_OK);
  free(runtime->ring); free(runtime->slots); free(runtime->driver);
  memset(runtime, 0, sizeof(*runtime));
  FixtureLedger ledger = fixture_ledger();
  CHECK(!ledger.allocations && !ledger.bytes && !ledger.overflow);
}
static uint64_t fixture_now(void* raw) { (void)raw; return ku_task_driver_now_ms(); }
static uint32_t fixture_resume(void* raw) {
  FixtureTask* task = (FixtureTask*)raw;
  FixtureWitness* witness = task->witness;
  fixture_increment(&witness->resumes);
  CHECK(task->frame_initialized);
  KuTaskFrameClockV1 clock = { fixture_now, task };
  uint32_t status = ku_task_frame_0_resume(task->frame, sizeof(*task->frame), FRAME_ABI, &clock);
  if (status == KU_TASK_FRAME_PENDING) {
    CHECK(ku_task_driver_set_intent(&task->ticket,
        task->registered_wait ? KU_TASK_DRIVER_WAIT : KU_TASK_DRIVER_YIELD) == KU_TASK_DRIVER_OK);
    if (task->block_pending) {
      CHECK(ku_test_event_set(&witness->entered));
      CHECK(ku_test_event_wait(&witness->proceed, 2000));
    }
    return KU_TASK_CONTROL_PENDING;
  }
  CHECK(status == KU_TASK_FRAME_READY && !task->payload_initialized);
  CHECK(ku_task_frame_0_take_result(task->frame, sizeof(*task->frame), FRAME_ABI, &task->payload) == KU_TASK_FRAME_OK);
  task->payload_initialized = true;
  return task->payload.ok ? KU_TASK_CONTROL_COMPLETED : KU_TASK_CONTROL_FAILED;
}
static uint32_t fixture_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {
  FixtureTask* task = (FixtureTask*)raw;
  FixtureWitness* witness = task->witness;
  fixture_increment(&witness->cleanups);
  if (task->block_cleanup) {
    CHECK(ku_test_event_set(&witness->entered));
    CHECK(ku_test_event_wait(&witness->proceed, 2000));
  }
  witness->cleanup_deadline = ku_task_control_cleanup_deadline(budget);
  if (!task->frame_initialized) return KU_TASK_CONTROL_OK;
  uint32_t frame_reason = reason == KU_TASK_CONTROL_TIMED_OUT ? KU_TASK_FRAME_TIMED_OUT : KU_TASK_FRAME_CANCELLED;
  KuTaskFrameClockV1 clock = { fixture_now, task };
  CHECK(ku_task_frame_0_terminate(task->frame, sizeof(*task->frame), FRAME_ABI,
      frame_reason, witness->cleanup_deadline, &clock) == frame_reason);
  return KU_TASK_CONTROL_OK;
}
static void fixture_drop_frame(void* raw) {
  FixtureTask* task = (FixtureTask*)raw;
  CHECK(task->frame && !fixture_count(&task->witness->frame_drops));
  if (task->frame_initialized)
    CHECK(ku_task_frame_0_destroy(task->frame, sizeof(*task->frame), FRAME_ABI) == KU_TASK_FRAME_OK);
  free(task->frame); task->frame = NULL;
  fixture_increment(&task->witness->frame_drops);
}
static void fixture_drop_payload(void* raw) {
  FixtureTask* task = (FixtureTask*)raw;
  CHECK(task->payload_initialized && !fixture_count(&task->witness->payload_drops));
  ku_result_drop_str(&task->payload); task->payload_initialized = false;
  fixture_increment(&task->witness->payload_drops);
}
static uint32_t fixture_take_payload(void* raw, void* output) {
  FixtureTask* task = (FixtureTask*)raw;
  KuResult_str* target = (KuResult_str*)output;
  if (!target || target->ok || target->value.ptr || target->error.message.ptr)
    return KU_TASK_CONTROL_INVALID_ARGUMENT;
  CHECK(task->payload_initialized);
  if (task->block_take) {
    if (task->early_take_notify)
      CHECK(ku_task_driver_wake(&task->ticket) == KU_TASK_DRIVER_OK);
    CHECK(ku_test_event_set(&task->witness->entered));
    CHECK(ku_test_event_wait(&task->witness->proceed, 2000));
  }
  *target = ku_result_move_str(&task->payload); task->payload_initialized = false;
  fixture_increment(&task->witness->takes);
  return KU_TASK_CONTROL_OK;
}
static void fixture_dispose(KuTaskControlV1* control, void* raw) {
  FixtureTask* task = (FixtureTask*)raw;
  FixtureWitness* witness = task->witness;
  KuTaskDriverTicketV1 ticket = task->ticket;
  bool block_dispose = task->block_dispose;
  CHECK(control == &task->control && !task->frame && !task->payload_initialized);
  CHECK(fixture_count(&witness->frame_drops) == 1 && !fixture_count(&witness->disposes));
  fixture_increment(&witness->disposes);
  free(task);
  if (block_dispose) {
    CHECK(ku_test_event_set(&witness->entered));
    CHECK(ku_test_event_wait(&witness->proceed, 2000));
  }
  /* Budget return reenters the driver AFTER the actual heap release, outside
   * the driver's lock. This catches both premature accounting and lock cycles. */
  CHECK(ku_task_driver_disposed(&ticket) == KU_TASK_DRIVER_OK);
  CHECK(ku_test_event_set(&witness->disposed));
}
static const KuTaskControlOpsV1 fixture_ops = {
  fixture_resume, fixture_cleanup, fixture_drop_frame,
  fixture_drop_payload, fixture_take_payload, fixture_dispose
};
enum { FIXTURE_NORMAL, FIXTURE_NO_ALLOCATION, FIXTURE_FAIL_CONTEXT,
       FIXTURE_FAIL_FRAME, FIXTURE_FAIL_CONTROL, FIXTURE_FAIL_FRAME_INIT, FIXTURE_ABORT_CONSUMED };
static uint32_t fixture_build(FixtureDriver* runtime, FixtureInputs* inputs, FixtureWitness* witness,
                             KuTaskControlOwnerV1* owner, KuTaskDriverTicketV1* ticket,
                             int fault, bool wait, bool block_pending, bool block_take, bool block_cleanup) {
  /* A test may pause an actual BUILDING reservation across shutdown, then
   * finish that same builder. It never manufactures a ticket or control. */
  size_t charged_bytes;
  if (!fixture_input_charge(inputs, &charged_bytes)) return KU_TASK_DRIVER_LIMIT;
  uint32_t reserved = ticket->driver ? KU_TASK_DRIVER_OK
      : ku_task_driver_reserve(runtime->driver, charged_bytes, ticket);
  if (reserved != KU_TASK_DRIVER_OK) return reserved;
  if (fault == FIXTURE_NO_ALLOCATION) {
    CHECK(ku_task_driver_rollback(ticket) == KU_TASK_DRIVER_OK);
    return KU_TASK_DRIVER_LIMIT;
  }
  if (fault == FIXTURE_FAIL_CONTEXT) fixture_fail_allocation(0);
  FixtureTask* task = (FixtureTask*)calloc(1, sizeof(*task));
  if (!task) {
    CHECK(ku_task_driver_rollback(ticket) == KU_TASK_DRIVER_OK);
    return KU_TASK_DRIVER_LIMIT;
  }
  if (fault == FIXTURE_FAIL_FRAME) fixture_fail_allocation(0);
  task->frame = (KuTaskFrame_0*)calloc(1, sizeof(*task->frame));
  if (!task->frame) {
    free(task); CHECK(ku_task_driver_rollback(ticket) == KU_TASK_DRIVER_OK);
    return KU_TASK_DRIVER_LIMIT;
  }
  task->ticket = *ticket; task->witness = witness;
  task->registered_wait = wait; task->block_pending = block_pending;
  task->block_take = block_take; task->early_take_notify = block_take;
  task->block_cleanup = block_cleanup;
  KuTaskControlOpsV1 operations = fixture_ops;
  if (fault == FIXTURE_FAIL_CONTROL) operations.resume = NULL;
  uint32_t initialized = ku_task_control_init(&task->control, sizeof(task->control),
      CONTROL_ABI, &operations, task, owner);
  if (fault == FIXTURE_FAIL_CONTROL) {
    CHECK(initialized == KU_TASK_CONTROL_INVALID_ARGUMENT && !owner->lease.control);
    free(task->frame); free(task);
    CHECK(ku_task_driver_rollback(ticket) == KU_TASK_DRIVER_OK);
    return KU_TASK_DRIVER_LIMIT;
  }
  CHECK(initialized == KU_TASK_CONTROL_OK);
  uint32_t framed = ku_task_frame_0_init(task->frame, sizeof(*task->frame),
      fault == FIXTURE_FAIL_FRAME_INIT ? 0 : FRAME_ABI, &inputs->result, &inputs->cleanup);
  task->frame_initialized = framed == KU_TASK_FRAME_OK;
  if (fault == FIXTURE_FAIL_FRAME_INIT) CHECK(framed == KU_TASK_FRAME_ABI_MISMATCH);
  else CHECK(framed == KU_TASK_FRAME_OK);
  uint32_t mode = fault == FIXTURE_FAIL_FRAME_INIT || fault == FIXTURE_ABORT_CONSUMED
      ? KU_TASK_DRIVER_ABORT : KU_TASK_DRIVER_START;
  return ku_task_driver_commit(ticket, owner, mode, fixture_deadline());
}
static void fixture_wait_disposed(FixtureWitness* witness) {
  CHECK(ku_test_event_wait(&witness->disposed, 2000));
  CHECK(fixture_count(&witness->disposes) == 1 && fixture_count(&witness->frame_drops) == 1);
}
static void fixture_retire(FixtureDriver* runtime, KuTaskDriverTicketV1* ticket,
                           KuTaskControlOwnerV1* owner, FixtureWitness* witness) {
  CHECK(ku_task_driver_owner_drop(ticket, owner, fixture_deadline()) == KU_TASK_DRIVER_OK);
  CHECK(!owner->lease.control);
  fixture_wait_disposed(witness);
  fixture_idle(runtime);
}
static void fixture_admission_rollback(void) {
  FixtureDriver runtime;
  fixture_driver_init(&runtime, 1, fixture_task_bytes());
  FixtureLedger base = fixture_ledger();
  KuTaskDriverTicketV1 ticket = {0}, second = {0};
  CHECK(ku_task_driver_reserve(runtime.driver, fixture_task_bytes() + 1, &ticket) == KU_TASK_DRIVER_LIMIT);
  CHECK(ku_task_driver_reserve(runtime.driver, SIZE_MAX, &ticket) == KU_TASK_DRIVER_LIMIT);
  CHECK(!fixture_snapshot(&runtime).resident);
  CHECK(ku_task_driver_reserve(runtime.driver, fixture_task_bytes(), &ticket) == KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_reserve(runtime.driver, 1, &second) == KU_TASK_DRIVER_LIMIT);
  KuTaskDriverSnapshotV1 snapshot = fixture_snapshot(&runtime);
  CHECK(snapshot.resident == 1 && snapshot.building == 1);
  CHECK(snapshot.reserved_bytes == fixture_task_bytes());
  CHECK(ku_task_driver_rollback(&ticket) == KU_TASK_DRIVER_OK);
  CHECK(!fixture_snapshot(&runtime).resident);
  {
    FixtureWitness witness; fixture_witness_init(&witness);
    FixtureInputs inputs = fixture_inputs();
    uint8_t* original = inputs.result.value.ptr;
    size_t original_capacity = inputs.result.value.capacity;
    inputs.result.value.capacity = SIZE_MAX;
    KuTaskControlOwnerV1 owner = {0};
    CHECK(fixture_build(&runtime, &inputs, &witness, &owner, &ticket,
        FIXTURE_NORMAL, false, false, false, false) == KU_TASK_DRIVER_LIMIT);
    CHECK(inputs.result.value.ptr == original && !owner.lease.control && !ticket.driver);
    CHECK(!fixture_snapshot(&runtime).resident);
    inputs.result.value.capacity = original_capacity;
    fixture_inputs_drop(&inputs); fixture_witness_finish(&witness);
  }
  for (int fault = FIXTURE_NO_ALLOCATION; fault <= FIXTURE_ABORT_CONSUMED; ++fault) {
    FixtureWitness witness; fixture_witness_init(&witness);
    FixtureInputs inputs = fixture_inputs();
    uint8_t* original = inputs.result.value.ptr;
    KuTaskControlOwnerV1 owner = {0};
    memset(&ticket, 0, sizeof(ticket));
    uint32_t built = fixture_build(&runtime, &inputs, &witness, &owner, &ticket,
        fault, false, false, false, false);
    if (fault < FIXTURE_FAIL_FRAME_INIT) CHECK(built == KU_TASK_DRIVER_LIMIT);
    else {
      CHECK(built == KU_TASK_DRIVER_OK && !owner.lease.control);
      fixture_wait_disposed(&witness);
      CHECK(!fixture_count(&witness.resumes) && fixture_count(&witness.cleanups) == 1);
    }
    if (fault == FIXTURE_ABORT_CONSUMED) CHECK(!inputs.result.value.ptr && !inputs.cleanup.ptr);
    else CHECK(inputs.result.value.ptr == original && inputs.cleanup.ptr);
    fixture_inputs_drop(&inputs); fixture_idle(&runtime);
    CHECK(!fixture_snapshot(&runtime).resident);
    FixtureLedger after = fixture_ledger();
    CHECK(after.allocations == base.allocations && after.bytes == base.bytes);
    fixture_witness_finish(&witness);
  }
  fixture_driver_finish(&runtime);
}
static void fixture_last_lease_budget_and_generation(void) {
  FixtureDriver runtime; fixture_driver_init(&runtime, 1, fixture_task_bytes());
  FixtureWitness witness; fixture_witness_init(&witness);
  FixtureInputs inputs = fixture_inputs();
  KuTaskControlOwnerV1 owner = {0}; KuTaskDriverTicketV1 ticket = {0};
  CHECK(fixture_build(&runtime, &inputs, &witness, &owner, &ticket,
      FIXTURE_NORMAL, false, false, false, false) == KU_TASK_DRIVER_OK);
  KuTaskControlLeaseV1 observer = {0};
  fixture_retain(&owner.lease, &observer);
  fixture_idle(&runtime);
  KuTaskDriverSnapshotV1 completed = fixture_snapshot(&runtime);
  CHECK(completed.resident == 1 && completed.terminal_held == 1);
  CHECK(fixture_count(&witness.resumes) == 2 && !fixture_count(&witness.disposes));
  CHECK(ku_task_driver_owner_drop(&ticket, &owner, fixture_deadline()) == KU_TASK_DRIVER_OK);
  fixture_idle(&runtime);
  CHECK(fixture_count(&witness.payload_drops) == 1 && !fixture_count(&witness.disposes));
  KuTaskDriverSnapshotV1 held = fixture_snapshot(&runtime);
  CHECK(held.resident == 1 && held.reserved_bytes == completed.reserved_bytes);
  KuTaskDriverTicketV1 refused = {0};
  CHECK(ku_task_driver_reserve(runtime.driver, fixture_task_bytes(), &refused) == KU_TASK_DRIVER_LIMIT);
  CHECK(ku_task_control_lease_release(&observer) == KU_TASK_CONTROL_OK);
  fixture_wait_disposed(&witness);
  CHECK(!fixture_snapshot(&runtime).resident);
  CHECK(ku_task_driver_wake(&ticket) == KU_TASK_DRIVER_STALE);
  KuTaskDriverTicketV1 reused = {0};
  CHECK(ku_task_driver_reserve(runtime.driver, fixture_task_bytes(), &reused) == KU_TASK_DRIVER_OK);
  CHECK(reused.slot == ticket.slot && reused.generation != ticket.generation);
  CHECK(ku_task_driver_wake(&ticket) == KU_TASK_DRIVER_STALE);
  CHECK(ku_task_driver_rollback(&reused) == KU_TASK_DRIVER_OK);
  fixture_witness_finish(&witness); fixture_driver_finish(&runtime);
}
static void fixture_duplicate_control_commit_preserves_first_owner(void) {
  FixtureDriver runtime; fixture_driver_init(&runtime, 2, 2 * fixture_task_bytes());
  FixtureWitness witness; fixture_witness_init(&witness);
  FixtureInputs inputs = fixture_inputs();
  KuTaskControlOwnerV1 owner = {0};
  KuTaskDriverTicketV1 first = {0}, duplicate = {0};
  CHECK(fixture_build(&runtime, &inputs, &witness, &owner, &first,
      FIXTURE_NORMAL, true, false, false, false) == KU_TASK_DRIVER_OK);
  fixture_idle(&runtime);
  CHECK(fixture_snapshot(&runtime).parked == 1 && fixture_count(&witness.resumes) == 1);
  KuTaskControlV1* original = owner.lease.control;
  size_t references = ku_task_control_atomic_load(&original->references);
  CHECK(ku_task_driver_reserve(runtime.driver, fixture_task_bytes(), &duplicate) == KU_TASK_DRIVER_OK);
  CHECK(duplicate.slot != first.slot);
  CHECK(ku_task_driver_commit(&duplicate, &owner, KU_TASK_DRIVER_START, fixture_deadline()) == KU_TASK_DRIVER_INVALID_STATE);
  CHECK(owner.lease.control == original);
  CHECK(ku_task_control_atomic_load(&original->references) == references);
  KuTaskDriverSnapshotV1 rejected = fixture_snapshot(&runtime);
  CHECK(rejected.resident == 2 && rejected.building == 1 && rejected.parked == 1);
  CHECK(rejected.reserved_bytes == 2 * fixture_task_bytes());
  CHECK(fixture_count(&witness.resumes) == 1 && !fixture_count(&witness.frame_drops));
  CHECK(ku_task_driver_rollback(&duplicate) == KU_TASK_DRIVER_OK && !duplicate.driver);
  CHECK(fixture_snapshot(&runtime).resident == 1);
  CHECK(ku_task_driver_wake(&first) == KU_TASK_DRIVER_OK);
  fixture_idle(&runtime);
  CHECK(fixture_count(&witness.resumes) == 2);
  fixture_retire(&runtime, &first, &owner, &witness);
  KuTaskDriverSnapshotV1 retired = fixture_snapshot(&runtime);
  CHECK(!retired.resident && !retired.reserved_bytes);
  fixture_witness_finish(&witness); fixture_driver_finish(&runtime);
}
static void fixture_retiring_binding_cleared_before_budget_return(void) {
  FixtureDriver runtime; fixture_driver_init(&runtime, 2, 2 * fixture_task_bytes());
  FixtureWitness old_witness, new_witness;
  fixture_witness_init(&old_witness); fixture_witness_init(&new_witness);
  FixtureInputs old_inputs = fixture_inputs();
  KuTaskControlOwnerV1 old_owner = {0}, new_owner = {0};
  KuTaskDriverTicketV1 old_ticket = {0}, new_ticket = {0};
  CHECK(fixture_build(&runtime, &old_inputs, &old_witness, &old_owner, &old_ticket,
      FIXTURE_NORMAL, false, false, false, false) == KU_TASK_DRIVER_OK);
  fixture_idle(&runtime);
  /* Owner protects the context, and no disposal can start before its transfer. */
  ((FixtureTask*)old_owner.lease.control->context)->block_dispose = true;
  CHECK(ku_task_driver_owner_drop(&old_ticket, &old_owner, fixture_deadline()) == KU_TASK_DRIVER_OK);
  CHECK(ku_test_event_wait(&old_witness.entered, 2000));
  /* Context/control have really been freed, but the dispose budget callback is
   * paused. Inspect only the still-live driver's protected tombstone. */
  KuTaskDriverSnapshotV1 gap = fixture_snapshot(&runtime);
  CHECK(gap.resident == 1 && gap.retiring == 1 && gap.reserved_bytes == fixture_task_bytes());
  CHECK(ku_task_driver_lock(runtime.driver) == 0);
  KuTaskDriverSlotV1* slot = &runtime.slots[old_ticket.slot];
  CHECK(slot->state == KU_TASK_DRIVER_RETIRING && slot->generation == old_ticket.generation);
  CHECK(slot->binding == NULL && !slot->driver_lease.control && !slot->execution_lease.control);
  CHECK(ku_task_driver_unlock(runtime.driver) == 0);
  FixtureInputs new_inputs = fixture_inputs();
  CHECK(fixture_build(&runtime, &new_inputs, &new_witness, &new_owner, &new_ticket,
      FIXTURE_NORMAL, false, false, false, false) == KU_TASK_DRIVER_OK);
  CHECK(new_ticket.slot != old_ticket.slot);
  gap = fixture_snapshot(&runtime);
  CHECK(gap.resident == 2 && gap.retiring == 1 && gap.reserved_bytes == 2 * fixture_task_bytes());
  CHECK(ku_test_event_set(&old_witness.proceed));
  fixture_wait_disposed(&old_witness);
  fixture_idle(&runtime);
  CHECK(fixture_count(&new_witness.resumes) == 2);
  fixture_retire(&runtime, &new_ticket, &new_owner, &new_witness);
  fixture_witness_finish(&old_witness); fixture_witness_finish(&new_witness);
  fixture_driver_finish(&runtime);
}
typedef struct FixtureWakeWorker { KuTaskDriverTicketV1 ticket; unsigned count; } FixtureWakeWorker;
static int fixture_wake_worker(void* raw) {
  FixtureWakeWorker* worker = (FixtureWakeWorker*)raw;
  for (unsigned index = 0; index < worker->count; ++index)
    CHECK(ku_task_driver_wake(&worker->ticket) == KU_TASK_DRIVER_OK);
  return 0;
}
static void fixture_deduplicated_wakes(bool running, bool retire_running) {
  FixtureDriver runtime; fixture_driver_init(&runtime, 1, fixture_task_bytes());
  FixtureWitness witness; fixture_witness_init(&witness);
  FixtureInputs inputs = fixture_inputs();
  KuTaskControlOwnerV1 owner = {0}; KuTaskDriverTicketV1 ticket = {0};
  CHECK(fixture_build(&runtime, &inputs, &witness, &owner, &ticket,
      FIXTURE_NORMAL, true, running, false, false) == KU_TASK_DRIVER_OK);
  if (running) CHECK(ku_test_event_wait(&witness.entered, 2000));
  else {
    fixture_idle(&runtime);
    CHECK(fixture_snapshot(&runtime).parked == 1 && fixture_count(&witness.resumes) == 1);
  }
  FixtureWakeWorker first = { ticket, 128 }, second = { ticket, 128 };
  KuTestThread left, right;
  CHECK(ku_test_thread_start(&left, fixture_wake_worker, &first));
  CHECK(ku_test_thread_start(&right, fixture_wake_worker, &second));
  CHECK(ku_test_thread_join(&left, 2000) && left.outcome == 0);
  CHECK(ku_test_thread_join(&right, 2000) && right.outcome == 0);
  if (retire_running) {
    CHECK(ku_task_driver_owner_drop(&ticket, &owner, fixture_deadline()) == KU_TASK_DRIVER_OK);
    CHECK(!owner.lease.control);
  }
  if (running) CHECK(ku_test_event_set(&witness.proceed));
  fixture_idle(&runtime);
  if (retire_running) {
    fixture_wait_disposed(&witness);
    CHECK(fixture_count(&witness.resumes) == 1 && fixture_count(&witness.cleanups) == 1);
  } else {
    CHECK(fixture_count(&witness.resumes) == 2);
    fixture_retire(&runtime, &ticket, &owner, &witness);
  }
  fixture_witness_finish(&witness); fixture_driver_finish(&runtime);
}
typedef struct FixtureTakeWorker {
  KuTaskDriverTicketV1 ticket;
  KuTaskControlLeaseV1 lease;
  KuResult_str output;
  uint32_t result;
} FixtureTakeWorker;
static int fixture_take_worker(void* raw) {
  FixtureTakeWorker* worker = (FixtureTakeWorker*)raw;
  worker->result = ku_task_driver_take_result(&worker->ticket, &worker->lease, &worker->output);
  return 0;
}
static void fixture_take_publication_notifies_retirement(void) {
  FixtureDriver runtime; fixture_driver_init(&runtime, 1, fixture_task_bytes());
  FixtureWitness witness; fixture_witness_init(&witness);
  FixtureInputs inputs = fixture_inputs();
  KuTaskControlOwnerV1 owner = {0}; KuTaskDriverTicketV1 ticket = {0};
  CHECK(fixture_build(&runtime, &inputs, &witness, &owner, &ticket,
      FIXTURE_NORMAL, false, false, true, false) == KU_TASK_DRIVER_OK);
  FixtureTakeWorker worker = {0}; worker.ticket = ticket;
  fixture_retain(&owner.lease, &worker.lease);
  fixture_idle(&runtime);
  KuTestThread thread;
  CHECK(ku_test_thread_start(&thread, fixture_take_worker, &worker));
  CHECK(ku_test_event_wait(&witness.entered, 2000));
  CHECK(ku_task_driver_owner_drop(&ticket, &owner, fixture_deadline()) == KU_TASK_DRIVER_OK);
  CHECK(!owner.lease.control);
  /* The callback already sent an intentionally EARLY wake while R2 was TAKING.
   * Driver must now wait, not spin, with owner retained in durable slot storage. */
  fixture_idle(&runtime);
  CHECK(fixture_snapshot(&runtime).resident == 1 && !fixture_count(&witness.disposes));
  CHECK(ku_test_event_set(&witness.proceed));
  CHECK(ku_test_thread_join(&thread, 2000) && thread.outcome == 0);
  CHECK(worker.result == KU_TASK_CONTROL_OK && worker.output.ok && worker.output.value.len == 7);
  fixture_idle(&runtime);
  CHECK(fixture_count(&witness.takes) == 1 && !fixture_count(&witness.payload_drops));
  CHECK(!fixture_count(&witness.disposes));
  CHECK(ku_task_control_lease_release(&worker.lease) == KU_TASK_CONTROL_OK);
  fixture_wait_disposed(&witness);
  ku_result_drop_str(&worker.output);
  fixture_witness_finish(&witness); fixture_driver_finish(&runtime);
}
static void fixture_error_result_and_caller_budget(void) {
  FixtureDriver runtime; fixture_driver_init(&runtime, 1, fixture_task_bytes() + 64);
  FixtureWitness witness; fixture_witness_init(&witness);
  FixtureInputs inputs = {0};
  inputs.result.error.domain = fixture_owned("task");
  inputs.result.error.code = fixture_owned("failure");
  inputs.result.error.message = fixture_owned("owned-error");
  inputs.cleanup = fixture_owned("cleanup");
  KuTaskControlOwnerV1 owner = {0}; KuTaskDriverTicketV1 ticket = {0};
  CHECK(fixture_build(&runtime, &inputs, &witness, &owner, &ticket,
      FIXTURE_NORMAL, false, false, false, false) == KU_TASK_DRIVER_OK);
  fixture_idle(&runtime);
  KuResult_str output = {0};
  CHECK(ku_task_driver_take_result(&ticket, &owner.lease, &output) == KU_TASK_CONTROL_OK);
  CHECK(!output.ok && output.error.domain.len == 4 && output.error.code.len == 7 && output.error.message.len == 11);
  KuResult_str untouched = {0};
  CHECK(ku_task_driver_take_result(&ticket, &owner.lease, &untouched) == KU_TASK_CONTROL_RESULT_TAKEN);
  CHECK(!untouched.ok && !untouched.error.message.ptr);
  fixture_retire(&runtime, &ticket, &owner, &witness);
  CHECK(fixture_count(&witness.takes) == 1 && !fixture_count(&witness.payload_drops));
  CHECK(!fixture_snapshot(&runtime).reserved_bytes);
  /* A taken value now belongs to the caller, not the retired task's budget.
   * Its real heap allocation deliberately outlives control disposal here. */
  CHECK(output.error.message.len == 11 && output.error.message.ptr[0] == 'o');
  ku_result_drop_str(&output);
  fixture_witness_finish(&witness); fixture_driver_finish(&runtime);
}
typedef struct FixtureShutdownWorker {
  KuTaskDriverV1* driver;
  uint64_t deadline;
  uint32_t result;
} FixtureShutdownWorker;
static int fixture_shutdown_worker(void* raw) {
  FixtureShutdownWorker* worker = (FixtureShutdownWorker*)raw;
  worker->result = ku_task_driver_shutdown(worker->driver, worker->deadline);
  return 0;
}
static void fixture_building_blocks_shutdown(bool commit_after_closing) {
  FixtureDriver runtime; fixture_driver_init(&runtime, 1, fixture_task_bytes());
  KuTaskDriverTicketV1 building = {0};
  CHECK(ku_task_driver_reserve(runtime.driver, fixture_task_bytes(), &building) == KU_TASK_DRIVER_OK);
  CHECK(fixture_snapshot(&runtime).building == 1);
  FixtureShutdownWorker worker = { runtime.driver, ku_task_driver_now_ms() + 30, 0 };
  KuTestThread thread;
  CHECK(ku_test_thread_start(&thread, fixture_shutdown_worker, &worker));
  CHECK(ku_test_thread_join(&thread, 2000) && thread.outcome == 0);
  CHECK(worker.result == KU_TASK_DRIVER_SHUTDOWN_TIMEOUT);
  KuTaskDriverSnapshotV1 snapshot = fixture_snapshot(&runtime);
  CHECK(snapshot.closing && snapshot.building == 1 && snapshot.resident == 1);
  CHECK(ku_task_driver_destroy(runtime.driver) == KU_TASK_DRIVER_PENDING);
  KuTaskDriverTicketV1 refused = {0};
  CHECK(ku_task_driver_reserve(runtime.driver, 1, &refused) == KU_TASK_DRIVER_CLOSED);
  if (commit_after_closing) {
    FixtureWitness witness; fixture_witness_init(&witness);
    FixtureInputs inputs = fixture_inputs();
    KuTaskControlOwnerV1 owner = {0};
    CHECK(fixture_build(&runtime, &inputs, &witness, &owner, &building,
        FIXTURE_NORMAL, false, false, false, false) == KU_TASK_DRIVER_OK);
    CHECK(!inputs.result.value.ptr && !inputs.cleanup.ptr);
    CHECK(ku_task_driver_owner_drop(&building, &owner, fixture_deadline()) == KU_TASK_DRIVER_OK);
    fixture_wait_disposed(&witness);
    CHECK(!fixture_count(&witness.resumes) && fixture_count(&witness.cleanups) == 1);
    fixture_witness_finish(&witness);
  } else CHECK(ku_task_driver_rollback(&building) == KU_TASK_DRIVER_OK);
  fixture_idle(&runtime);
  fixture_driver_finish(&runtime);
}
static void fixture_shared_deadline_and_blocked_shutdown(void) {
  FixtureDriver runtime; fixture_driver_init(&runtime, 2, 2 * fixture_task_bytes());
  FixtureWitness witnesses[2];
  KuTaskControlOwnerV1 owners[2] = {0};
  KuTaskDriverTicketV1 tickets[2] = {0};
  for (unsigned index = 0; index < 2; ++index) {
    fixture_witness_init(&witnesses[index]);
    FixtureInputs inputs = fixture_inputs();
    CHECK(fixture_build(&runtime, &inputs, &witnesses[index], &owners[index], &tickets[index],
        FIXTURE_NORMAL, true, false, false, index == 0) == KU_TASK_DRIVER_OK);
  }
  fixture_idle(&runtime);
  CHECK(fixture_snapshot(&runtime).parked == 2);
  uint64_t owner_deadline = ku_task_driver_now_ms() + 5000;
  CHECK(ku_task_driver_owner_drop(&tickets[0], &owners[0], owner_deadline) == KU_TASK_DRIVER_OK);
  CHECK(ku_test_event_wait(&witnesses[0].entered, 2000));
  CHECK(ku_task_driver_owner_drop(&tickets[1], &owners[1], owner_deadline) == KU_TASK_DRIVER_OK);
  FixtureShutdownWorker worker = { runtime.driver, ku_task_driver_now_ms() + 30, 0 };
  KuTestThread thread;
  CHECK(ku_test_thread_start(&thread, fixture_shutdown_worker, &worker));
  CHECK(ku_test_thread_join(&thread, 2000) && thread.outcome == 0);
  CHECK(worker.result == KU_TASK_DRIVER_SHUTDOWN_TIMEOUT);
  KuTaskDriverSnapshotV1 blocked = fixture_snapshot(&runtime);
  CHECK(blocked.closing && blocked.resident == 2 && blocked.running == 1);
  CHECK(ku_task_driver_destroy(runtime.driver) == KU_TASK_DRIVER_PENDING);
  CHECK(!fixture_count(&witnesses[0].disposes) && !fixture_count(&witnesses[1].disposes));
  CHECK(ku_test_event_set(&witnesses[0].proceed));
  for (unsigned index = 0; index < 2; ++index) {
    fixture_wait_disposed(&witnesses[index]);
    CHECK(fixture_count(&witnesses[index].resumes) == 1);
    /* The blocked first callback entered with owner_deadline. Shutdown
     * tightened it while running, so R2 must reconcile once more before its
     * terminal commit. The second task first runs with the final shared D. */
    CHECK(fixture_count(&witnesses[index].cleanups) == (index == 0 ? 2u : 1u));
    CHECK(witnesses[index].cleanup_deadline == worker.deadline);
    fixture_witness_finish(&witnesses[index]);
  }
  fixture_idle(&runtime); fixture_driver_finish(&runtime);
}
static void fixture_shutdown_with_reference_saturation(void) {
  FixtureDriver runtime; fixture_driver_init(&runtime, 1, fixture_task_bytes());
  FixtureWitness witness; fixture_witness_init(&witness);
  FixtureInputs inputs = fixture_inputs();
  KuTaskControlOwnerV1 owner = {0}; KuTaskDriverTicketV1 ticket = {0};
  CHECK(fixture_build(&runtime, &inputs, &witness, &owner, &ticket,
      FIXTURE_NORMAL, true, true, false, false) == KU_TASK_DRIVER_OK);
  CHECK(ku_test_event_wait(&witness.entered, 2000));
  KuTaskControlLeaseV1* leases = (KuTaskControlLeaseV1*)calloc(KU_TASK_CONTROL_MAX_REFERENCES, sizeof(*leases));
  CHECK(leases);
  size_t retained = 0;
  for (; retained < KU_TASK_CONTROL_MAX_REFERENCES; ++retained) {
    uint32_t result = ku_task_control_lease_retain(&owner.lease, &leases[retained]);
    if (result == KU_TASK_CONTROL_LIMIT) break;
    CHECK(result == KU_TASK_CONTROL_OK);
  }
  CHECK(retained > 0 && retained < KU_TASK_CONTROL_MAX_REFERENCES);
  CHECK(ku_task_control_atomic_load(&owner.lease.control->references) == KU_TASK_CONTROL_MAX_REFERENCES);
  FixtureShutdownWorker worker = { runtime.driver, ku_task_driver_now_ms() + 30, 0 };
  KuTestThread thread;
  CHECK(ku_test_thread_start(&thread, fixture_shutdown_worker, &worker));
  CHECK(ku_test_thread_join(&thread, 2000) && thread.outcome == 0);
  CHECK(worker.result == KU_TASK_DRIVER_SHUTDOWN_TIMEOUT);
  /* No spare reference exists and resume is still blocked. Cancellation must
   * already be durable, rather than silently skipped after retain hit LIMIT. */
  CHECK(ku_task_control_atomic_load(&owner.lease.control->phase) == KU_TASK_CONTROL_REQUESTED_CANCEL);
  CHECK(ku_task_driver_destroy(runtime.driver) == KU_TASK_DRIVER_PENDING);
  for (size_t index = 0; index < retained; ++index)
    CHECK(ku_task_control_lease_release(&leases[index]) == KU_TASK_CONTROL_OK);
  free(leases);
  CHECK(ku_task_driver_owner_drop(&ticket, &owner, fixture_deadline()) == KU_TASK_DRIVER_OK);
  CHECK(!owner.lease.control);
  CHECK(ku_test_event_set(&witness.proceed));
  fixture_wait_disposed(&witness);
  CHECK(fixture_count(&witness.resumes) == 1 && fixture_count(&witness.cleanups) == 1);
  fixture_idle(&runtime);
  fixture_witness_finish(&witness); fixture_driver_finish(&runtime);
}
#if defined(__linux__)
/* Failure is sticky, so the public idle/shutdown APIs correctly return INTERNAL
 * instead of certifying success. Observe the real driver's condition predicate
 * with a healthy caller clock; no sleep, guessed scheduling, or forged state. */
static KuTaskDriverSnapshotV1 fixture_fault_quiescent(FixtureDriver* runtime, bool exited) {
  CHECK(ku_task_driver_lock(runtime->driver) == 0);
  uint64_t deadline = fixture_deadline();
  while (runtime->driver->queued || runtime->driver->running
      || (exited ? !runtime->driver->worker_exited : !runtime->driver->worker_waiting)) {
    CHECK(ku_task_driver_wait(runtime->driver, deadline) >= 0);
    CHECK(ku_task_driver_now_ms() < deadline);
  }
  CHECK(ku_task_driver_unlock(runtime->driver) == 0);
  KuTaskDriverSnapshotV1 snapshot;
  CHECK(ku_task_driver_snapshot(runtime->driver, &snapshot) == KU_TASK_DRIVER_OK);
  CHECK(snapshot.fault == KU_TASK_DRIVER_INTERNAL && snapshot.clock_fault && snapshot.closing);
  CHECK(!snapshot.queued && !snapshot.running);
  CHECK(exited ? snapshot.worker_exited : snapshot.worker_waiting && !snapshot.worker_exited);
  return snapshot;
}
static void fixture_fault_finish(FixtureDriver* runtime) {
  KuTaskDriverSnapshotV1 snapshot = fixture_fault_quiescent(runtime, true);
  CHECK(!snapshot.resident && !snapshot.building && !snapshot.reserved_bytes);
  CHECK(!snapshot.parked && !snapshot.retiring && !snapshot.terminal_held);
  CHECK(ku_task_driver_wait_idle(runtime->driver, fixture_deadline()) == KU_TASK_DRIVER_INTERNAL);
  CHECK(ku_task_driver_shutdown(runtime->driver, fixture_deadline()) == KU_TASK_DRIVER_INTERNAL);
  CHECK(ku_task_driver_destroy(runtime->driver) == KU_TASK_DRIVER_OK);
  free(runtime->ring); free(runtime->slots); free(runtime->driver);
  memset(runtime, 0, sizeof(*runtime));
  FixtureLedger ledger = fixture_ledger();
  CHECK(!ledger.allocations && !ledger.bytes && !ledger.overflow);
}
#endif
static void fixture_clock_failure_is_bounded(void) {
#if defined(__linux__)
  FixtureDriver runtime; fixture_driver_init(&runtime, 1, fixture_task_bytes());
  KuTaskDriverTicketV1 building = {0};
  CHECK(ku_task_driver_reserve(runtime.driver, fixture_task_bytes(), &building) == KU_TASK_DRIVER_OK);
  /* Per-thread injection cannot nondeterministically fault the idle worker on
   * an unrelated condition-variable spurious wake. Only these entry calls fail. */
  fixture_clock_failure = 1;
  CHECK(ku_task_driver_now_ms() == UINT64_MAX);
  CHECK(ku_task_driver_lock(runtime.driver) == 0);
  CHECK(ku_task_driver_wait(runtime.driver, UINT64_MAX) == -2);
  CHECK(ku_task_driver_unlock(runtime.driver) == 0);
  CHECK(ku_task_driver_shutdown(runtime.driver, UINT64_MAX) == KU_TASK_DRIVER_INTERNAL);
  CHECK(ku_task_driver_destroy(runtime.driver) == KU_TASK_DRIVER_PENDING);
  fixture_clock_failure = 0;
  KuTaskDriverSnapshotV1 snapshot = fixture_fault_quiescent(&runtime, false);
  CHECK(snapshot.resident == 1 && snapshot.building == 1);
  CHECK(snapshot.reserved_bytes == fixture_task_bytes());
  CHECK(ku_task_driver_rollback(&building) == KU_TASK_DRIVER_OK);
  fixture_fault_finish(&runtime);
#endif
}
static void fixture_worker_clock_failure_drains_without_recovery(void) {
#if defined(__linux__)
  FixtureDriver runtime; fixture_driver_init(&runtime, 2, 2 * fixture_task_bytes());
  FixtureWitness first, second;
  fixture_witness_init(&first); fixture_witness_init(&second);
  KuTestEvent fault; CHECK(ku_test_event_init(&fault));
  KuTaskControlOwnerV1 first_owner = {0}, second_owner = {0};
  KuTaskDriverTicketV1 first_ticket = {0}, building = {0};
  FixtureInputs first_inputs = fixture_inputs();
  CHECK(fixture_build(&runtime, &first_inputs, &first, &first_owner, &first_ticket,
      FIXTURE_NORMAL, true, false, false, false) == KU_TASK_DRIVER_OK);
  fixture_idle(&runtime);
  CHECK(ku_task_driver_reserve(runtime.driver, fixture_task_bytes(), &building) == KU_TASK_DRIVER_OK);
  /* The real frame is already PARKED. Holding the registry mutex excludes a
   * worker dequeue while this separately retained lease performs R2's bounded,
   * callback-free cancellation. Arm failure before unlocking, then use the
   * ordinary wake wrapper. The worker's NEXT clock sample must fail before its
   * next poll, not after R2 cleaned up within an already-running quantum. */
  KuTaskControlLeaseV1 timeout_lease = {0};
  fixture_retain(&first_owner.lease, &timeout_lease);
  CHECK(ku_task_driver_lock(runtime.driver) == 0);
  CHECK(runtime.slots[first_ticket.slot].state == KU_TASK_DRIVER_PARKED);
  CHECK(ku_task_control_request_cancel(&timeout_lease,
      KU_TASK_CONTROL_TIMED_OUT, fixture_deadline()) == KU_TASK_CONTROL_OK);
  fixture_worker_clock_failed = &fault;
  ku_task_control_atomic_store(&fixture_worker_clock_failure, 1);
  CHECK(ku_task_driver_unlock(runtime.driver) == 0);
  CHECK(ku_task_driver_wake(&first_ticket) == KU_TASK_DRIVER_OK);
  CHECK(ku_task_control_lease_release(&timeout_lease) == KU_TASK_CONTROL_OK);
  CHECK(ku_test_event_wait(&fault, 2000));
  KuTaskDriverSnapshotV1 snapshot = fixture_fault_quiescent(&runtime, false);
  CHECK(snapshot.resident == 2 && snapshot.building == 1 && snapshot.terminal_held == 1);
  CHECK(snapshot.reserved_bytes == 2 * fixture_task_bytes());
  CHECK(fixture_count(&first.resumes) == 1 && fixture_count(&first.cleanups) == 1);
  CHECK(fixture_count(&first.frame_drops) == 1 && !fixture_count(&first.disposes));
  CHECK(first.cleanup_deadline == 0);
  CHECK(ku_task_control_status(&first_owner.lease) == KU_TASK_CONTROL_TIMED_OUT);
  CHECK(ku_task_driver_wait_idle(runtime.driver, fixture_deadline()) == KU_TASK_DRIVER_INTERNAL);
  CHECK(ku_task_driver_shutdown(runtime.driver, fixture_deadline()) == KU_TASK_DRIVER_INTERNAL);
  CHECK(ku_task_driver_destroy(runtime.driver) == KU_TASK_DRIVER_PENDING);
  KuTaskDriverTicketV1 rejected = {0};
  CHECK(ku_task_driver_reserve(runtime.driver, 1, &rejected) == KU_TASK_DRIVER_CLOSED);
  CHECK(!rejected.driver);

  /* A builder that held admission before failure may still commit its fully
   * initialized frame. It must perform cleanup only, never ordinary resume. */
  FixtureInputs second_inputs = fixture_inputs();
  CHECK(fixture_build(&runtime, &second_inputs, &second, &second_owner, &building,
      FIXTURE_NORMAL, true, false, false, false) == KU_TASK_DRIVER_OK);
  snapshot = fixture_fault_quiescent(&runtime, false);
  CHECK(snapshot.resident == 2 && !snapshot.building && snapshot.terminal_held == 2);
  CHECK(!fixture_count(&second.resumes) && fixture_count(&second.cleanups) == 1);
  CHECK(fixture_count(&second.frame_drops) == 1 && !fixture_count(&second.disposes));
  CHECK(second.cleanup_deadline == 0);
  CHECK(ku_task_control_status(&second_owner.lease) == KU_TASK_CONTROL_CANCELLED);

  /* Keep worker clock failure enabled through BOTH durable owner transfers,
   * actual heap disposal, clockless idle wake, and final worker exit. */
  CHECK(ku_task_driver_owner_drop(&first_ticket, &first_owner, fixture_deadline()) == KU_TASK_DRIVER_OK);
  CHECK(!first_owner.lease.control);
  fixture_wait_disposed(&first);
  snapshot = fixture_fault_quiescent(&runtime, false);
  CHECK(snapshot.resident == 1 && snapshot.terminal_held == 1);
  CHECK(snapshot.reserved_bytes == fixture_task_bytes());
  CHECK(ku_task_driver_owner_drop(&building, &second_owner, fixture_deadline()) == KU_TASK_DRIVER_OK);
  CHECK(!second_owner.lease.control);
  fixture_wait_disposed(&second);
  fixture_fault_finish(&runtime);
  ku_task_control_atomic_store(&fixture_worker_clock_failure, 0);
  fixture_worker_clock_failed = NULL;
  CHECK(ku_test_event_destroy(&fault));
  fixture_witness_finish(&first); fixture_witness_finish(&second);
#endif
}
static void fixture_init_and_default_capacity_bounds(void) {
  KuTaskDriverV1 driver;
  KuTaskDriverSlotV1 slot;
  size_t ring = 0;
  memset(&driver, 0, sizeof(driver)); memset(&slot, 0, sizeof(slot));
  size_t fixed = sizeof(driver) + sizeof(slot) + sizeof(ring);
  CHECK(ku_task_driver_init(&driver, sizeof(driver), 0, &slot, 1, &ring, 1, fixed) == KU_TASK_DRIVER_ABI_MISMATCH);
  CHECK(ku_task_driver_init(&driver, sizeof(driver), DRIVER_ABI, &slot, 0, &ring, 0, fixed) == KU_TASK_DRIVER_LIMIT);
  CHECK(ku_task_driver_init(&driver, sizeof(driver), DRIVER_ABI, &slot, SIZE_MAX, &ring, SIZE_MAX, SIZE_MAX) == KU_TASK_DRIVER_LIMIT);
  CHECK(ku_task_driver_init(&driver, sizeof(driver), DRIVER_ABI, &slot, KU_TASK_DRIVER_MAX_SLOTS + 1, &ring, KU_TASK_DRIVER_MAX_SLOTS + 1, SIZE_MAX) == KU_TASK_DRIVER_LIMIT);
  CHECK(ku_task_driver_init(&driver, sizeof(driver), DRIVER_ABI, &slot, 1, &ring, 0, fixed) == KU_TASK_DRIVER_LIMIT);
  CHECK(ku_task_driver_init(&driver, sizeof(driver), DRIVER_ABI, &slot, 1, &ring, 1, fixed - 1) == KU_TASK_DRIVER_LIMIT);
  CHECK(ku_task_driver_init(&driver, sizeof(driver) - 1, DRIVER_ABI, &slot, 1, &ring, 1, fixed) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(ku_task_driver_init((KuTaskDriverV1*)((char*)&driver + 1), sizeof(driver), DRIVER_ABI, &slot, 1, &ring, 1, fixed) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(ku_task_driver_init(&driver, sizeof(driver), DRIVER_ABI, &slot, 1, (size_t*)&driver, 1, fixed) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(ku_task_frame_zero_bytes(&driver, sizeof(driver)));
  FixtureDriver runtime;
  fixture_driver_init(&runtime, KU_TASK_DRIVER_MAX_SLOTS, KU_TASK_DRIVER_MAX_SLOTS * fixture_task_bytes());
  KuTaskDriverTicketV1* tickets = (KuTaskDriverTicketV1*)calloc(KU_TASK_DRIVER_MAX_SLOTS, sizeof(*tickets));
  CHECK(tickets);
  for (size_t index = 0; index < KU_TASK_DRIVER_MAX_SLOTS; ++index)
    CHECK(ku_task_driver_reserve(runtime.driver, fixture_task_bytes(), &tickets[index]) == KU_TASK_DRIVER_OK);
  KuTaskDriverTicketV1 extra = {0};
  CHECK(ku_task_driver_reserve(runtime.driver, fixture_task_bytes(), &extra) == KU_TASK_DRIVER_LIMIT);
  KuTaskDriverSnapshotV1 full = fixture_snapshot(&runtime);
  CHECK(full.resident == KU_TASK_DRIVER_MAX_SLOTS && full.building == full.resident);
  CHECK(full.reserved_bytes == KU_TASK_DRIVER_MAX_SLOTS * fixture_task_bytes());
  for (size_t index = KU_TASK_DRIVER_MAX_SLOTS; index != 0; --index)
    CHECK(ku_task_driver_rollback(&tickets[index - 1]) == KU_TASK_DRIVER_OK);
  free(tickets);
  fixture_driver_finish(&runtime);
}
int main(void) {
  fixture_linux_absolute_deadline();
  fixture_clock_failure_is_bounded();
  fixture_worker_clock_failure_drains_without_recovery();
  fixture_init_and_default_capacity_bounds();
  fixture_admission_rollback();
  fixture_duplicate_control_commit_preserves_first_owner();
  fixture_retiring_binding_cleared_before_budget_return();
  fixture_error_result_and_caller_budget();
  fixture_building_blocks_shutdown(false);
  fixture_building_blocks_shutdown(true);
  fixture_shared_deadline_and_blocked_shutdown();
  fixture_shutdown_with_reference_saturation();
  for (unsigned round = 0; round < 16; ++round) {
    fixture_last_lease_budget_and_generation();
    fixture_deduplicated_wakes(false, false);
    fixture_deduplicated_wakes(true, false);
    fixture_deduplicated_wakes(true, true);
    fixture_take_publication_notifies_retirement();
  }
  puts("task-driver-v1-ok");
  return 0;
}
"#;
