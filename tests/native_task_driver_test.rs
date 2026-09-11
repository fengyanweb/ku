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
    fixture_source_with_backedge(false)
}

fn fixture_source_with_backedge(backedge: bool) -> String {
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
                        resume: StateId(if backedge { 0 } else { 1 }),
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
        .find("static int ku_task_driver_wait_on(")
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
            "static void ku_task_driver_worker(KuTaskDriverWorkerV1* worker) {",
            "static void ku_task_driver_worker(KuTaskDriverWorkerV1* worker) {\n#if defined(__linux__)\nfixture_is_driver_worker = 1;\n#endif",
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
#define ku_task_driver_wait_on fixture_generated_deadline_wait_on
#define ku_task_driver_wait_work fixture_generated_deadline_wait_work
#define ku_task_driver_now_ms fixture_wait_now_ms
#define clock_gettime fixture_wait_clock_gettime
#define pthread_cond_timedwait fixture_wait_cond_timedwait
#define pthread_cond_wait fixture_wait_cond_wait
"#;

const LINUX_DEADLINE_TAIL: &str = r#"
#undef ku_task_driver_wait
#undef ku_task_driver_wait_on
#undef ku_task_driver_wait_work
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
static uint64_t fixture_driver_one_second(void) {
  uint64_t now = ku_task_driver_now_ms();
  CHECK(now != UINT64_MAX && now < UINT64_MAX - 1000u);
  return now + 1000u;
}
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
  CHECK(snapshot.worker_target == 1u && snapshot.workers_created == 1u);
  CHECK(snapshot.workers_waiting + snapshot.workers_exited <= 1u);
  CHECK(snapshot.resident <= runtime->capacity && snapshot.building <= snapshot.resident);
  CHECK(!snapshot.fault);
  return snapshot;
}
static void fixture_idle(FixtureDriver* runtime) {
  CHECK(ku_task_driver_wait_idle(runtime->driver, fixture_deadline()) == KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot = fixture_snapshot(runtime);
  CHECK(!snapshot.queued && !snapshot.running
      && snapshot.workers_waiting + snapshot.workers_exited == snapshot.workers_created);
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
  uint64_t startup_deadline = fixture_driver_one_second();
  CHECK(ku_task_driver_init(runtime->driver, sizeof(*runtime->driver), DRIVER_ABI,
      runtime->slots, capacity, runtime->ring, capacity, runtime->fixed_bytes + task_budget,
      1, startup_deadline) == KU_TASK_DRIVER_OK);
  fixture_idle(runtime);
  KuTaskDriverSnapshotV1 snapshot = fixture_snapshot(runtime);
  CHECK(!snapshot.resident && snapshot.fixed_bytes == runtime->fixed_bytes);
}
static void fixture_driver_reap(FixtureDriver* runtime, uint64_t deadline) {
#if defined(_WIN32)
  /* Some fault scenarios deliberately release their owner after D expired.
   * After all last-access witnesses, this separate OS observation can wait for
   * thread return. It neither joins/closes a handle nor extends runtime D, and
   * is not evidence that those scenarios completed cleanup before D. */
  uint64_t now = ku_task_driver_now_ms(); CHECK(now != UINT64_MAX);
  if (now >= deadline) {
    uint64_t observed_until = fixture_deadline();
    for (size_t index = 0; index < runtime->driver->workers_created; index++) {
      now = ku_task_driver_now_ms(); CHECK(now != UINT64_MAX);
      DWORD remaining = now < observed_until ? (DWORD)(observed_until - now) : 0u;
      CHECK(WaitForSingleObject(runtime->driver->workers[index].thread, remaining) == WAIT_OBJECT_0);
    }
  }
#endif
  CHECK(ku_task_driver_join(runtime->driver, deadline) == KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_destroy(runtime->driver) == KU_TASK_DRIVER_OK);
}
static void fixture_driver_finish(FixtureDriver* runtime) {
  uint64_t deadline = fixture_driver_one_second();
  CHECK(ku_task_driver_shutdown(runtime->driver, deadline) == KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot = fixture_snapshot(runtime);
  CHECK(!snapshot.resident && !snapshot.building && !snapshot.queued && !snapshot.running);
  CHECK(!snapshot.parked && !snapshot.retiring && !snapshot.terminal_held);
  CHECK(snapshot.workers_exited == snapshot.workers_created);
  CHECK(ku_task_driver_lock(runtime->driver) == 0);
  deadline = runtime->driver->shutdown_deadline;
  CHECK(ku_task_driver_unlock(runtime->driver) == 0);
  fixture_driver_reap(runtime, deadline);
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
      || (exited ? runtime->driver->workers_exited != runtime->driver->workers_created
          : runtime->driver->workers_waiting != runtime->driver->workers_created)) {
    CHECK(ku_task_driver_wait(runtime->driver, deadline) >= 0);
    CHECK(ku_task_driver_now_ms() < deadline);
  }
  CHECK(ku_task_driver_unlock(runtime->driver) == 0);
  KuTaskDriverSnapshotV1 snapshot;
  CHECK(ku_task_driver_snapshot(runtime->driver, &snapshot) == KU_TASK_DRIVER_OK);
  CHECK(snapshot.fault == KU_TASK_DRIVER_INTERNAL && snapshot.clock_fault && snapshot.closing);
  CHECK(!snapshot.queued && !snapshot.running);
  CHECK(snapshot.worker_target == 1u && snapshot.workers_created == 1u);
  CHECK(exited ? snapshot.workers_exited == 1u
      : snapshot.workers_waiting == 1u && !snapshot.workers_exited);
  return snapshot;
}
static void fixture_fault_finish(FixtureDriver* runtime) {
  KuTaskDriverSnapshotV1 snapshot = fixture_fault_quiescent(runtime, true);
  CHECK(!snapshot.resident && !snapshot.building && !snapshot.reserved_bytes);
  CHECK(!snapshot.parked && !snapshot.retiring && !snapshot.terminal_held);
  CHECK(ku_task_driver_wait_idle(runtime->driver, fixture_deadline()) == KU_TASK_DRIVER_INTERNAL);
  CHECK(ku_task_driver_shutdown(runtime->driver, fixture_deadline()) == KU_TASK_DRIVER_INTERNAL);
  CHECK(ku_task_driver_lock(runtime->driver) == 0);
  uint64_t deadline = runtime->driver->shutdown_deadline;
  CHECK(ku_task_driver_unlock(runtime->driver) == 0);
  CHECK(deadline == 0);
  fixture_driver_reap(runtime, deadline);
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
  uint64_t startup_deadline = fixture_driver_one_second();
  CHECK(KU_TASK_DRIVER_ABI_VERSION == 7u);
  for (uint32_t old = 0; old < KU_TASK_DRIVER_ABI_VERSION; old++)
    CHECK(ku_task_driver_init(&driver, sizeof(driver), old, &slot, 1, &ring, 1, fixed, 1, startup_deadline) == KU_TASK_DRIVER_ABI_MISMATCH);
  CHECK(ku_task_driver_init(&driver, sizeof(driver), DRIVER_ABI, &slot, 0, &ring, 0, fixed, 1, startup_deadline) == KU_TASK_DRIVER_LIMIT);
  CHECK(ku_task_driver_init(&driver, sizeof(driver), DRIVER_ABI, &slot, SIZE_MAX, &ring, SIZE_MAX, SIZE_MAX, 1, startup_deadline) == KU_TASK_DRIVER_LIMIT);
  CHECK(ku_task_driver_init(&driver, sizeof(driver), DRIVER_ABI, &slot, KU_TASK_DRIVER_MAX_SLOTS + 1, &ring, KU_TASK_DRIVER_MAX_SLOTS + 1, SIZE_MAX, 1, startup_deadline) == KU_TASK_DRIVER_LIMIT);
  CHECK(ku_task_driver_init(&driver, sizeof(driver), DRIVER_ABI, &slot, 1, &ring, 0, fixed, 1, startup_deadline) == KU_TASK_DRIVER_LIMIT);
  CHECK(ku_task_driver_init(&driver, sizeof(driver), DRIVER_ABI, &slot, 1, &ring, 1, fixed - 1, 1, startup_deadline) == KU_TASK_DRIVER_LIMIT);
  CHECK(ku_task_driver_init(&driver, sizeof(driver) - 1, DRIVER_ABI, &slot, 1, &ring, 1, fixed, 1, startup_deadline) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(ku_task_driver_init((KuTaskDriverV1*)((char*)&driver + 1), sizeof(driver), DRIVER_ABI, &slot, 1, &ring, 1, fixed, 1, startup_deadline) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(ku_task_driver_init(&driver, sizeof(driver), DRIVER_ABI, &slot, 1, (size_t*)&driver, 1, fixed, 1, startup_deadline) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(ku_task_driver_init(&driver, sizeof(driver), DRIVER_ABI, &slot, 1, &ring, 1, fixed, 0, startup_deadline) == KU_TASK_DRIVER_LIMIT);
  CHECK(ku_task_driver_init(&driver, sizeof(driver), DRIVER_ABI, &slot, 1, &ring, 1, fixed, 33, startup_deadline) == KU_TASK_DRIVER_LIMIT);
  CHECK(ku_task_driver_init(&driver, sizeof(driver), DRIVER_ABI, &slot, 1, &ring, 1, fixed, 1, UINT64_MAX) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(ku_task_driver_init(&driver, sizeof(driver), DRIVER_ABI, &slot, 1, &ring, 1, fixed, 1, 0) == KU_TASK_DRIVER_SHUTDOWN_TIMEOUT);
  CHECK(ku_task_frame_zero_bytes(&driver, sizeof(driver)));
  CHECK(ku_task_frame_zero_bytes(&slot, sizeof(slot)) && !ring);
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

#[test]
fn native_task_driver_two_real_workers_overlap_without_same_task_overlap() {
    let generated = fixture_source();
    assert_eq!(generated.matches("typedef struct KuString {").count(), 1);
    assert_eq!(generated.matches("int main(void) {").count(), 1);
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
        );
    // Reuse the real frame/control builder, but not the old one-worker
    // snapshot/init/teardown helpers or their separate lifecycle scenarios.
    let definitions_end = C_MAIN
        .find("static KuTaskDriverSnapshotV1 fixture_snapshot(")
        .expect("shared driver fixture definitions");
    let callbacks_start = C_MAIN
        .find("static uint64_t fixture_now(")
        .expect("shared driver frame callbacks");
    let callbacks_end = C_MAIN
        .find("static void fixture_wait_disposed(")
        .expect("end of shared driver builder");
    let callbacks = &C_MAIN[callbacks_start..callbacks_end];
    let resume_entry = "fixture_resume, fixture_cleanup, fixture_drop_frame,";
    assert_eq!(callbacks.matches(resume_entry).count(), 1);
    source.push_str(&C_MAIN[..definitions_end]);
    source.push_str("static uint32_t fixture_parallel_resume(void* raw);\n");
    source.push_str(&callbacks.replacen(
        resume_entry,
        "fixture_parallel_resume, fixture_cleanup, fixture_drop_frame,",
        1,
    ));
    source.push_str(TWO_WORKER_MAIN);
    let directory = TempDir::new("task-driver-two-worker-overlap");
    let c_file = directory.path().join("parallel.c");
    fs::write(&c_file, source).expect("write two-worker driver witness");
    let Some(executable) = compile_harness(directory.path(), &c_file, "parallel") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute the two-worker driver witness"
        );
        return;
    };
    fs::remove_file(&c_file).expect("remove C source before running parallel witness");
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("two-worker witness and cleanup must remain bounded");
    assert!(
        output.status.success(),
        "two-worker witness failed (ABI 6 must reach its cleanup marker before controlled red):\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-driver-two-worker-cleanup-ok\ntask-driver-two-worker-overlap-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const TWO_WORKER_MAIN: &str = r#"
#if defined(_WIN32)
typedef DWORD FixtureParallelThreadId;
static FixtureParallelThreadId fixture_parallel_thread_id(void) { return GetCurrentThreadId(); }
static int fixture_parallel_same_thread(FixtureParallelThreadId a, FixtureParallelThreadId b) { return a == b; }
#else
typedef pthread_t FixtureParallelThreadId;
static FixtureParallelThreadId fixture_parallel_thread_id(void) { return pthread_self(); }
static int fixture_parallel_same_thread(FixtureParallelThreadId a, FixtureParallelThreadId b) { return pthread_equal(a,b); }
#endif
typedef struct FixtureParallelProbe {
  FixtureWitness witness;
  size_t active, maximum_active, calls;
  int identity_set;
  FixtureParallelThreadId identity;
} FixtureParallelProbe;
static FixtureParallelProbe fixture_parallel_probes[2];
static FixtureParallelThreadId fixture_parallel_controller;
static int fixture_parallel_gate_timeout;
/* Probe fields use the existing fixture mutex, never held while waiting or
 * invoking the frame. This short bookkeeping lock cannot serialize the gates. */
static uint32_t fixture_parallel_resume(void* raw) {
  FixtureTask* task = (FixtureTask*)raw;
  size_t index = task->witness == &fixture_parallel_probes[0].witness ? 0 : 1;
  FixtureParallelProbe* probe = &fixture_parallel_probes[index];
  CHECK(task->witness == &probe->witness);
  fixture_alloc_lock();
  probe->active++;
  if (probe->active > probe->maximum_active) probe->maximum_active = probe->active;
  int first = ++probe->calls == 1;
  if (first) { probe->identity = fixture_parallel_thread_id(); probe->identity_set = 1; }
  fixture_alloc_unlock();
  if (first) {
    CHECK(ku_test_event_set(&probe->witness.entered));
    /* The controller releases both gates after ONE shared 2 s observation
     * window. This longer fail-safe prevents an ABI 6 callback from timing out
     * at that same boundary; it is not a performance acceptance threshold. */
    int released = ku_test_event_wait(&probe->witness.proceed, 5000);
    if (!released) {
      fixture_alloc_lock(); fixture_parallel_gate_timeout = 1; fixture_alloc_unlock();
    }
  }
  uint32_t outcome = fixture_resume(raw);
  fixture_alloc_lock();
  CHECK(probe->active);
  probe->active--;
  fixture_alloc_unlock();
  return outcome;
}
static uint64_t fixture_parallel_one_second_deadline(void) {
  uint64_t now = ku_task_driver_now_ms();
  CHECK(now != UINT64_MAX && now < UINT64_MAX-1000u);
  return now+1000u; /* Finite D; neither overflow nor the MAX sentinel. */
}
static unsigned long fixture_parallel_remaining(uint64_t deadline) {
  uint64_t now = ku_task_driver_now_ms();
  if (now == UINT64_MAX || now >= deadline) return 0;
  uint64_t left = deadline - now;
  return left > (uint64_t)ULONG_MAX ? ULONG_MAX : (unsigned long)left;
}
static void fixture_parallel_init(FixtureDriver* runtime, uint64_t startup_deadline) {
  memset(runtime,0,sizeof(*runtime));
  runtime->capacity = 2;
  runtime->driver = (KuTaskDriverV1*)calloc(1,sizeof(*runtime->driver));
  runtime->slots = (KuTaskDriverSlotV1*)calloc(2,sizeof(*runtime->slots));
  runtime->ring = (size_t*)calloc(2,sizeof(*runtime->ring));
  CHECK(runtime->driver && runtime->slots && runtime->ring);
  runtime->fixed_bytes = sizeof(*runtime->driver) + 2*(sizeof(*runtime->slots)+sizeof(*runtime->ring));
  CHECK(fixture_task_bytes() <= (SIZE_MAX-runtime->fixed_bytes)/2);
#if KU_TASK_DRIVER_ABI_VERSION >= 7u
  CHECK(ku_task_driver_init(runtime->driver,sizeof(*runtime->driver),KU_TASK_DRIVER_ABI_VERSION,
      runtime->slots,2,runtime->ring,2,runtime->fixed_bytes+2*fixture_task_bytes(),
      2,startup_deadline) == KU_TASK_DRIVER_OK);
#else
  (void)startup_deadline;
  /* Real baseline: the unchanged ABI 6 driver creates its sole OS worker. */
  CHECK(ku_task_driver_init(runtime->driver,sizeof(*runtime->driver),KU_TASK_DRIVER_ABI_VERSION,
      runtime->slots,2,runtime->ring,2,runtime->fixed_bytes+2*fixture_task_bytes()) == KU_TASK_DRIVER_OK);
#endif
}
static void fixture_parallel_join_destroy(FixtureDriver* runtime, uint64_t deadline) {
#if KU_TASK_DRIVER_ABI_VERSION >= 7u
  CHECK(ku_task_driver_join(runtime->driver,deadline) == KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_destroy(runtime->driver) == KU_TASK_DRIVER_OK);
#else
  /* ABI 6 has no group join API. Its successful shutdown is followed by the
   * actual single handle acknowledgement, still using the same teardown D. */
#if defined(_WIN32)
  CHECK(WaitForSingleObject(runtime->driver->thread,(DWORD)fixture_parallel_remaining(deadline)) == WAIT_OBJECT_0);
#else
  unsigned long remaining = fixture_parallel_remaining(deadline);
  CHECK(remaining);
  alarm((unsigned int)((remaining+999UL)/1000UL));
#endif
  CHECK(ku_task_driver_destroy(runtime->driver) == KU_TASK_DRIVER_OK);
#if !defined(_WIN32)
  alarm(0);
#endif
#endif
  free(runtime->ring); free(runtime->slots); free(runtime->driver);
  memset(runtime,0,sizeof(*runtime));
}
int main(void) {
  fixture_parallel_controller = fixture_parallel_thread_id();
  for (size_t i=0;i<2;i++) fixture_witness_init(&fixture_parallel_probes[i].witness);
  FixtureDriver runtime;
  const uint64_t startup_deadline = fixture_parallel_one_second_deadline();
  fixture_parallel_init(&runtime,startup_deadline);
  const uint64_t startup_finished = ku_task_driver_now_ms();
  CHECK(startup_finished != UINT64_MAX && startup_finished <= startup_deadline);
  KuTaskControlOwnerV1 owners[2] = {{0}};
  KuTaskDriverTicketV1 tickets[2] = {{0}};
  for (size_t i=0;i<2;i++) {
    FixtureInputs inputs = fixture_inputs();
    CHECK(fixture_build(&runtime,&inputs,&fixture_parallel_probes[i].witness,
        &owners[i],&tickets[i],FIXTURE_NORMAL,false,false,false,false) == KU_TASK_DRIVER_OK);
    CHECK(!inputs.result.value.ptr && !inputs.cleanup.ptr);
    fixture_inputs_drop(&inputs);
  }
  /* Exercise queued/RUNNING wake coalescing without creating an executor or
   * ever calling control_poll from this controller. */
  for (unsigned n=0;n<8;n++) for (size_t i=0;i<2;i++)
    CHECK(ku_task_driver_wake(&tickets[i]) == KU_TASK_DRIVER_OK);
  const uint64_t observation_deadline = fixture_deadline();
  int first_entered = ku_test_event_wait(&fixture_parallel_probes[0].witness.entered,
      fixture_parallel_remaining(observation_deadline));
  int second_entered = ku_test_event_wait(&fixture_parallel_probes[1].witness.entered,
      fixture_parallel_remaining(observation_deadline));
  fixture_alloc_lock();
  int parallel = first_entered && second_entered
      && fixture_parallel_probes[0].active == 1 && fixture_parallel_probes[1].active == 1
      && fixture_parallel_probes[0].identity_set && fixture_parallel_probes[1].identity_set
      && !fixture_parallel_same_thread(fixture_parallel_probes[0].identity,fixture_parallel_probes[1].identity)
      && !fixture_parallel_same_thread(fixture_parallel_probes[0].identity,fixture_parallel_controller)
      && !fixture_parallel_same_thread(fixture_parallel_probes[1].identity,fixture_parallel_controller);
  fixture_alloc_unlock();
  KuTaskDriverSnapshotV1 blocked = {0};
  CHECK(ku_task_driver_snapshot(runtime.driver,&blocked) == KU_TASK_DRIVER_OK);
  parallel = parallel && blocked.running == 2 && !blocked.queued && blocked.resident == 2 && !blocked.fault;
  /* Crucially, absence of overlap is not asserted until both real tasks and
   * all real workers have been released and reclaimed. Manual-reset gates also
   * release the ABI 6 second callback which cannot enter until this point. */
  const uint64_t cleanup_deadline = fixture_parallel_one_second_deadline();
  CHECK(ku_test_event_set(&fixture_parallel_probes[0].witness.proceed));
  CHECK(ku_test_event_set(&fixture_parallel_probes[1].witness.proceed));
  for (size_t i=0;i<2;i++) {
    CHECK(ku_task_driver_wait_result(&tickets[i],&owners[i].lease,cleanup_deadline) == KU_TASK_DRIVER_WAIT_READY);
    CHECK(ku_task_control_status(&owners[i].lease) == KU_TASK_CONTROL_COMPLETED);
    CHECK(ku_task_driver_owner_drop(&tickets[i],&owners[i],cleanup_deadline) == KU_TASK_DRIVER_OK);
    CHECK(!owners[i].lease.control);
  }
  for (size_t i=0;i<2;i++)
    CHECK(ku_test_event_wait(&fixture_parallel_probes[i].witness.disposed,
        fixture_parallel_remaining(cleanup_deadline)));
  CHECK(ku_task_driver_shutdown(runtime.driver,cleanup_deadline) == KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_lock(runtime.driver) == 0);
  const uint64_t stored_deadline = runtime.driver->shutdown_deadline;
  CHECK(ku_task_driver_unlock(runtime.driver) == 0);
  CHECK(stored_deadline == cleanup_deadline);
  KuTaskDriverSnapshotV1 finished = {0};
  CHECK(ku_task_driver_snapshot(runtime.driver,&finished) == KU_TASK_DRIVER_OK);
  CHECK(!finished.resident && !finished.building && !finished.queued && !finished.running
      && !finished.parked && !finished.retiring && !finished.terminal_held
      && !finished.reserved_bytes && !finished.fault);
  fixture_parallel_join_destroy(&runtime,cleanup_deadline);
  fixture_alloc_lock();
  int serialized = fixture_parallel_probes[0].maximum_active == 1
      && fixture_parallel_probes[1].maximum_active == 1
      && !fixture_parallel_probes[0].active && !fixture_parallel_probes[1].active;
  int gate_timeout = fixture_parallel_gate_timeout;
  fixture_alloc_unlock();
  for (size_t i=0;i<2;i++) {
    FixtureWitness* witness = &fixture_parallel_probes[i].witness;
    CHECK(fixture_count(&witness->resumes) == 2 && !fixture_count(&witness->cleanups));
    CHECK(fixture_count(&witness->frame_drops) == 1 && fixture_count(&witness->payload_drops) == 1
        && fixture_count(&witness->disposes) == 1 && !fixture_count(&witness->takes));
    fixture_witness_finish(witness);
  }
  FixtureLedger ledger = fixture_ledger();
  CHECK(!ledger.allocations && !ledger.bytes && !ledger.overflow);
  const uint64_t cleanup_finished = ku_task_driver_now_ms();
  CHECK(cleanup_finished != UINT64_MAX && cleanup_finished <= cleanup_deadline);
  puts("task-driver-two-worker-cleanup-ok");
  if (!parallel || !serialized || gate_timeout) {
    fprintf(stderr,"task-driver-two-worker-not-parallel-after-cleanup: overlap=%d serialized=%d gate_timeout=%d\n",
        parallel,serialized,gate_timeout);
    return 1;
  }
  puts("task-driver-two-worker-overlap-ok");
  return 0;
}
"#;

#[test]
fn native_task_driver_shutdown_observer_reloads_a_concurrent_shorter_deadline() {
    let generated = fixture_source();
    let wait_entry = "static int ku_task_driver_wait(KuTaskDriverV1* driver, uint64_t deadline) {";
    assert_eq!(generated.matches(wait_entry).count(), 1);
    assert_eq!(generated.matches("typedef struct KuString {").count(), 1);
    assert_eq!(generated.matches("int main(void) {").count(), 1);
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
            wait_entry,
            "static int ku_task_driver_wait(KuTaskDriverV1* driver, uint64_t deadline);\n\
             static int fixture_shutdown_real_wait(KuTaskDriverV1* driver, uint64_t deadline) {",
            1,
        );
    // Reuse fixture types/ledger helpers, not its one-worker lifecycle helper
    // or unrelated frame callbacks. The held object is a real reservation.
    let definitions_end = C_MAIN
        .find("static KuTaskDriverSnapshotV1 fixture_snapshot(")
        .expect("shared driver fixture definitions");
    source.push_str(&C_MAIN[..definitions_end]);
    source.push_str(SHUTDOWN_TIGHTENING_MAIN);
    let directory = TempDir::new("task-driver-shutdown-tightening");
    let c_file = directory.path().join("shutdown.c");
    fs::write(&c_file, source).expect("write concurrent shutdown observer witness");
    let Some(executable) = compile_harness(directory.path(), &c_file, "shutdown") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute the concurrent shutdown observer witness"
        );
        return;
    };
    fs::remove_file(&c_file).expect("remove C source before running shutdown witness");
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("shutdown observation and real resource reaping must remain bounded");
    assert!(
        output.status.success(),
        "shutdown observer witness failed (ABI 6 must reach cleanup before controlled red):\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-driver-shutdown-tighten-cleanup-ok\ntask-driver-shutdown-tighten-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const SHUTDOWN_TIGHTENING_MAIN: &str = r#"
enum { FIXTURE_SHUTDOWN_A=1u, FIXTURE_SHUTDOWN_B=2u };
static KU_THREAD_LOCAL unsigned fixture_shutdown_role;
static KuTestEvent fixture_shutdown_a_first, fixture_shutdown_a_after_b;
/* Only accessed with the real driver mutex held. Unrelated state broadcasts
 * may wake A before B; count the first actual wait entry AFTER B shortened D. */
static size_t fixture_shutdown_a_entries, fixture_shutdown_a_returns;
static int fixture_shutdown_b_marked, fixture_shutdown_captured;
static uint64_t fixture_shutdown_a_initial, fixture_shutdown_b_passed;
static uint64_t fixture_shutdown_b_stored, fixture_shutdown_a_passed, fixture_shutdown_a_stored;
static int ku_task_driver_wait(KuTaskDriverV1* driver, uint64_t deadline) {
  if (fixture_shutdown_role == FIXTURE_SHUTDOWN_A) {
    fixture_shutdown_a_entries++;
    if (fixture_shutdown_a_entries == 1) {
      fixture_shutdown_a_initial = deadline;
      CHECK(ku_test_event_set(&fixture_shutdown_a_first));
    }
    if (fixture_shutdown_b_marked && fixture_shutdown_a_returns && !fixture_shutdown_captured) {
      fixture_shutdown_a_passed = deadline;
      fixture_shutdown_a_stored = driver->shutdown_deadline;
      fixture_shutdown_captured = 1;
      CHECK(ku_test_event_set(&fixture_shutdown_a_after_b));
    }
  } else if (fixture_shutdown_role == FIXTURE_SHUTDOWN_B && !fixture_shutdown_b_marked) {
    fixture_shutdown_b_passed = deadline;
    fixture_shutdown_b_stored = driver->shutdown_deadline;
    fixture_shutdown_b_marked = 1;
  }
  /* Never fabricate a return or release/reacquire the driver mutex ourselves:
   * every intercepted entry executes the original native wait implementation. */
  int waited = fixture_shutdown_real_wait(driver,deadline);
  if (fixture_shutdown_role == FIXTURE_SHUTDOWN_A) fixture_shutdown_a_returns++;
  return waited;
}
typedef struct FixtureShutdownObserver {
  KuTaskDriverV1* driver;
  unsigned role;
  uint64_t deadline;
  KuTestEvent ready, begin;
} FixtureShutdownObserver;
static uint64_t fixture_shutdown_checked_now(void) {
  uint64_t now = ku_task_driver_now_ms();
  CHECK(now != UINT64_MAX && now < UINT64_MAX-1000u);
  return now;
}
static unsigned long fixture_shutdown_remaining(uint64_t deadline) {
  uint64_t now = ku_task_driver_now_ms();
  if (now == UINT64_MAX || now >= deadline) return 0;
  uint64_t remaining = deadline-now;
  return remaining > (uint64_t)ULONG_MAX ? ULONG_MAX : (unsigned long)remaining;
}
static int fixture_shutdown_observer(void* raw) {
  FixtureShutdownObserver* observer = (FixtureShutdownObserver*)raw;
  fixture_shutdown_role = observer->role;
  CHECK(ku_test_event_set(&observer->ready));
  CHECK(ku_test_event_wait(&observer->begin,2000));
  /* Main publishes deadline before signaling begin. No later argument write. */
  return (int)ku_task_driver_shutdown(observer->driver,observer->deadline);
}
static void fixture_shutdown_join_observer(KuTestThread* thread, uint64_t deadline) {
  CHECK(ku_test_event_wait(&thread->finished,fixture_shutdown_remaining(deadline)));
#if defined(_WIN32)
  CHECK(WaitForSingleObject(thread->handle,(DWORD)fixture_shutdown_remaining(deadline)) == WAIT_OBJECT_0);
  CHECK(CloseHandle(thread->handle)); thread->handle = NULL;
#else
  /* Real native join after the callback/finished witness; never a fake ACK. */
  CHECK(fixture_shutdown_remaining(deadline));
  CHECK(!pthread_join(thread->handle,NULL));
#endif
  CHECK(ku_test_event_destroy(&thread->finished));
}
static void fixture_shutdown_runtime_init(FixtureDriver* runtime, uint64_t startup_deadline) {
  memset(runtime,0,sizeof(*runtime));
  runtime->capacity = 1;
  runtime->driver = (KuTaskDriverV1*)calloc(1,sizeof(*runtime->driver));
  runtime->slots = (KuTaskDriverSlotV1*)calloc(1,sizeof(*runtime->slots));
  runtime->ring = (size_t*)calloc(1,sizeof(*runtime->ring));
  CHECK(runtime->driver && runtime->slots && runtime->ring);
  runtime->fixed_bytes = sizeof(*runtime->driver)+sizeof(*runtime->slots)+sizeof(*runtime->ring);
  CHECK(fixture_task_bytes() <= SIZE_MAX-runtime->fixed_bytes);
#if KU_TASK_DRIVER_ABI_VERSION >= 7u
  CHECK(ku_task_driver_init(runtime->driver,sizeof(*runtime->driver),KU_TASK_DRIVER_ABI_VERSION,
      runtime->slots,1,runtime->ring,1,runtime->fixed_bytes+fixture_task_bytes(),
      1,startup_deadline) == KU_TASK_DRIVER_OK);
#else
  (void)startup_deadline;
  CHECK(ku_task_driver_init(runtime->driver,sizeof(*runtime->driver),KU_TASK_DRIVER_ABI_VERSION,
      runtime->slots,1,runtime->ring,1,runtime->fixed_bytes+fixture_task_bytes()) == KU_TASK_DRIVER_OK);
#endif
}
static void fixture_shutdown_runtime_finish(FixtureDriver* runtime, uint64_t deadline) {
#if KU_TASK_DRIVER_ABI_VERSION >= 7u
  CHECK(ku_task_driver_join(runtime->driver,deadline) == KU_TASK_DRIVER_OK);
#elif defined(_WIN32)
  CHECK(WaitForSingleObject(runtime->driver->thread,(DWORD)fixture_shutdown_remaining(deadline)) == WAIT_OBJECT_0);
#else
  CHECK(fixture_shutdown_remaining(deadline));
#endif
  CHECK(ku_task_driver_destroy(runtime->driver) == KU_TASK_DRIVER_OK);
  free(runtime->ring); free(runtime->slots); free(runtime->driver);
  memset(runtime,0,sizeof(*runtime));
}
int main(void) {
  CHECK(ku_test_event_init(&fixture_shutdown_a_first));
  CHECK(ku_test_event_init(&fixture_shutdown_a_after_b));
  FixtureDriver runtime;
  const uint64_t startup_deadline = fixture_shutdown_checked_now()+1000u;
  fixture_shutdown_runtime_init(&runtime,startup_deadline);
  CHECK(fixture_shutdown_checked_now() <= startup_deadline);
  KuTaskDriverTicketV1 reservation = {0};
  CHECK(ku_task_driver_reserve(runtime.driver,fixture_task_bytes(),&reservation) == KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 building = {0};
  CHECK(ku_task_driver_snapshot(runtime.driver,&building) == KU_TASK_DRIVER_OK);
  CHECK(building.resident == 1 && building.building == 1 && !building.queued && !building.running);
  FixtureShutdownObserver observers[2] = {{0}};
  KuTestThread threads[2];
  for (size_t i=0;i<2;i++) {
    observers[i].driver = runtime.driver; observers[i].role = (unsigned)i+1u;
    CHECK(ku_test_event_init(&observers[i].ready));
    CHECK(ku_test_event_init(&observers[i].begin));
    CHECK(ku_test_thread_start(&threads[i],fixture_shutdown_observer,&observers[i]));
  }
  /* Setup observations precede the shared cleanup budget. Their 2 s ceiling
   * does not renew either shutdown call's later D. */
  for (size_t i=0;i<2;i++) CHECK(ku_test_event_wait(&observers[i].ready,2000));
  const uint64_t base = fixture_shutdown_checked_now();
  const uint64_t original_deadline = base+900u, shorter_deadline = base+700u;
  observers[0].deadline = original_deadline; observers[1].deadline = shorter_deadline;
  CHECK(ku_test_event_set(&observers[0].begin));
  int a_started = ku_test_event_wait(&fixture_shutdown_a_first,fixture_shutdown_remaining(shorter_deadline));
  /* A's first entry holds the driver mutex until its ORIGINAL OS wait releases
   * it. B therefore cannot shorten storage before A really starts waiting. */
  CHECK(ku_test_event_set(&observers[1].begin));
  int observed = ku_test_event_wait(&fixture_shutdown_a_after_b,fixture_shutdown_remaining(shorter_deadline));
  CHECK(ku_task_driver_lock(runtime.driver) == 0);
  int captured = fixture_shutdown_captured, b_marked = fixture_shutdown_b_marked;
  uint64_t initial = fixture_shutdown_a_initial, passed = fixture_shutdown_a_passed;
  uint64_t stored = fixture_shutdown_a_stored, b_passed = fixture_shutdown_b_passed;
  uint64_t b_stored = fixture_shutdown_b_stored;
  CHECK(ku_task_driver_unlock(runtime.driver) == 0);
  /* Always release the ACTUAL BUILDING reservation before verdict or join.
   * Neither observer is held by a test gate after its shutdown began. */
  CHECK(ku_task_driver_rollback(&reservation) == KU_TASK_DRIVER_OK);
  CHECK(!reservation.driver);
  for (size_t i=0;i<2;i++) fixture_shutdown_join_observer(&threads[i],shorter_deadline);
  const int a_outcome = threads[0].outcome, b_outcome = threads[1].outcome;
  CHECK(ku_task_driver_lock(runtime.driver) == 0);
  const uint64_t final_stored = runtime.driver->shutdown_deadline;
  CHECK(ku_task_driver_unlock(runtime.driver) == 0);
  KuTaskDriverSnapshotV1 finished = {0};
  CHECK(ku_task_driver_snapshot(runtime.driver,&finished) == KU_TASK_DRIVER_OK);
  CHECK(!finished.resident && !finished.building && !finished.queued && !finished.running
      && !finished.parked && !finished.retiring && !finished.terminal_held
      && !finished.reserved_bytes && !finished.fault);
  fixture_shutdown_runtime_finish(&runtime,shorter_deadline);
  for (size_t i=0;i<2;i++) {
    CHECK(ku_test_event_destroy(&observers[i].ready));
    CHECK(ku_test_event_destroy(&observers[i].begin));
  }
  CHECK(ku_test_event_destroy(&fixture_shutdown_a_first));
  CHECK(ku_test_event_destroy(&fixture_shutdown_a_after_b));
  FixtureLedger ledger = fixture_ledger();
  CHECK(!ledger.allocations && !ledger.bytes && !ledger.overflow);
  CHECK(fixture_shutdown_checked_now() <= shorter_deadline);
  CHECK(a_outcome == KU_TASK_DRIVER_OK && b_outcome == KU_TASK_DRIVER_OK);
  CHECK(a_started && observed && captured && b_marked && initial == original_deadline
      && b_passed == shorter_deadline && b_stored == shorter_deadline
      && stored == shorter_deadline && final_stored == shorter_deadline);
  puts("task-driver-shutdown-tighten-cleanup-ok");
  if (passed != stored) {
    fprintf(stderr,"task-driver-shutdown-stale-wait-deadline-after-cleanup: passed=%llu stored=%llu\n",
        (unsigned long long)passed,(unsigned long long)stored);
    return 1;
  }
  puts("task-driver-shutdown-tighten-ok");
  return 0;
}
"#;

#[test]
fn native_task_driver_empty_workers_keep_state_and_work_notifications_separate() {
    let generated = fixture_source();
    assert_eq!(generated.matches("typedef struct KuString {").count(), 1);
    assert_eq!(generated.matches("int main(void) {").count(), 1);
    let driver_start = generated
        .find("static uint64_t ku_task_driver_now_ms(void) {")
        .expect("generated driver clock entry");
    let destroy = generated[driver_start..]
        .find("static uint32_t ku_task_driver_destroy(")
        .expect("generated driver destroy");
    let driver_end = driver_start
        + destroy
        + generated[driver_start + destroy..]
            .find("\n}\n")
            .expect("end of generated driver destroy")
        + 3;
    let mut driver = generated[driver_start..driver_end].to_owned();
    // Intercept only this emitted driver's native calls. The test-event
    // implementation and every forwarded OS call remain unmodified.
    for (from, to, count) in [
        ("WakeAllConditionVariable(", "fixture_idle_os_signal(", 2),
        ("SleepConditionVariableSRW(", "fixture_idle_os_wait(", 2),
        ("pthread_cond_broadcast(", "fixture_idle_os_signal(", 2),
        ("pthread_cond_wait(", "fixture_idle_os_wait(", 2),
        (
            "pthread_cond_timedwait_relative_np(",
            "fixture_idle_os_relative_wait(",
            1,
        ),
        ("pthread_cond_timedwait(", "fixture_idle_os_timed_wait(", 1),
        (
            "static int ku_task_driver_wait_work(",
            "static int fixture_idle_real_wait_work(",
            1,
        ),
        (
            "static int ku_task_driver_wait(",
            "static int fixture_idle_real_wait(",
            1,
        ),
        (
            "static void ku_task_driver_worker(",
            "static void fixture_idle_real_worker(",
            1,
        ),
    ] {
        assert_eq!(driver.matches(from).count(), count, "{from}");
        driver = driver.replace(from, to);
    }
    let instrumentation = format!(
        "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\n{LOCKED_ALLOCATIONS}\n"
    );
    let mut source = generated[..driver_start].replacen(
        "typedef struct KuString {",
        &format!("{instrumentation}typedef struct KuString {{"),
        1,
    );
    source.push_str(EMPTY_ROUTE_PROBE);
    source.push_str(&driver);
    source.push_str(&generated[driver_end..].replacen(
        "int main(void) {",
        "static int ku_generated_main(void) {",
        1,
    ));
    let definitions_end = C_MAIN
        .find("static KuTaskDriverSnapshotV1 fixture_snapshot(")
        .expect("shared driver fixture definitions");
    source.push_str(&C_MAIN[..definitions_end]);
    source.push_str(EMPTY_ROUTE_MAIN);
    let directory = TempDir::new("task-driver-empty-routing");
    let c_file = directory.path().join("routing.c");
    fs::write(&c_file, source).expect("write real empty-worker routing witness");
    let Some(executable) = compile_harness(directory.path(), &c_file, "routing") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute the real empty-worker routing witness"
        );
        return;
    };
    fs::remove_file(&c_file).expect("remove C source before running routing witness");
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("real routing observation and cleanup must remain bounded");
    assert!(
        output.status.success(),
        "empty-worker routing witness failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap().replace('\r', "");
    assert!(stdout.starts_with("task-driver-empty-routing-ok "));
    assert_eq!(stdout.lines().count(), 1);
    assert!(output.stderr.is_empty());
}

const EMPTY_ROUTE_PROBE: &str = r#"
#define IDLE_CHECK(c) do { if (!(c)) { fprintf(stderr,"empty routing check line %d: %s\n",__LINE__,#c); abort(); } } while (0)
static int ku_task_driver_wait(KuTaskDriverV1*,uint64_t);
static int ku_task_driver_wait_work(KuTaskDriverV1*,uint64_t);
static void ku_task_driver_worker(KuTaskDriverWorkerV1*);
static KU_THREAD_LOCAL unsigned fixture_idle_role, fixture_idle_kind;
static KuTaskDriverV1* fixture_idle_driver;
static KuTestEvent fixture_idle_second_entered, fixture_idle_second_begin;
static KuTestEvent fixture_idle_first_wait[2], fixture_idle_reparked[2], fixture_idle_observer_wait;
static unsigned fixture_idle_phase, fixture_idle_bad_route, fixture_idle_seen[2];
static unsigned fixture_idle_in_wait[2], fixture_idle_repark_marked[2];
static uint64_t fixture_idle_entries[2], fixture_idle_returns[2];
static uint64_t fixture_idle_os_entries[2], fixture_idle_os_returns[2];
static uint64_t fixture_idle_phase_entries[2], fixture_idle_phase_returns[2];
static uint64_t fixture_idle_state_signals, fixture_idle_work_signals, fixture_idle_park_signals;
static uint64_t fixture_idle_observer_entries, fixture_idle_observer_returns;
#if defined(_WIN32)
static DWORD fixture_idle_ids[2];
#else
static pthread_t fixture_idle_ids[2];
#endif
/* Driver storage pointer is published before init starts actual threads, and
 * remains immutable until every native thread has joined. All shared counters
 * and phase/route fields are accessed under the driver's publication mutex. */
static void fixture_idle_count(uint64_t* count) {
  IDLE_CHECK(*count != UINT64_MAX); ++*count;
}
static void fixture_idle_signal_entry(KuTaskDriverConditionV1* condition) {
  KuTaskDriverV1* driver = fixture_idle_driver;
  IDLE_CHECK(driver && &driver->condition != &driver->work_condition);
  if (condition != &driver->condition && condition != &driver->work_condition)
    fixture_idle_bad_route = 1;
  if (fixture_idle_phase != 1) return;
  if (condition == &driver->work_condition) fixture_idle_count(&fixture_idle_work_signals);
  if (condition == &driver->condition) {
    fixture_idle_count(&fixture_idle_state_signals);
    if (fixture_idle_role == 1 || fixture_idle_role == 2)
      fixture_idle_count(&fixture_idle_park_signals);
  }
}
static void fixture_idle_wait_entry(KuTaskDriverConditionV1* condition, KuTaskDriverMutexV1* mutex) {
  KuTaskDriverV1* driver = fixture_idle_driver;
  IDLE_CHECK(driver);
  if (mutex != &driver->mutex) fixture_idle_bad_route = 1;
  if (fixture_idle_role == 1 || fixture_idle_role == 2) {
    unsigned i = fixture_idle_role-1;
    if (fixture_idle_kind != 1 || condition != &driver->work_condition)
      fixture_idle_bad_route = 1;
    IDLE_CHECK(!fixture_idle_in_wait[i]);
    fixture_idle_in_wait[i] = 1;
    fixture_idle_count(&fixture_idle_os_entries[i]);
    if (fixture_idle_os_entries[i] == 1) IDLE_CHECK(ku_test_event_set(&fixture_idle_first_wait[i]));
    if (fixture_idle_phase == 1) {
      fixture_idle_count(&fixture_idle_phase_entries[i]);
      if (!fixture_idle_repark_marked[i]) {
        fixture_idle_repark_marked[i] = 1;
        IDLE_CHECK(ku_test_event_set(&fixture_idle_reparked[i]));
      }
    }
  } else {
    if (fixture_idle_kind != 2 || condition != &driver->condition)
      fixture_idle_bad_route = 1;
    if (fixture_idle_role == 3) {
      fixture_idle_count(&fixture_idle_observer_entries);
      if (fixture_idle_observer_entries == 1)
        IDLE_CHECK(ku_test_event_set(&fixture_idle_observer_wait));
    }
  }
}
static void fixture_idle_wait_return(void) {
  if (fixture_idle_role == 1 || fixture_idle_role == 2) {
    unsigned i = fixture_idle_role-1;
    IDLE_CHECK(fixture_idle_in_wait[i]);
    fixture_idle_in_wait[i] = 0;
    fixture_idle_count(&fixture_idle_os_returns[i]);
    if (fixture_idle_phase == 1) fixture_idle_count(&fixture_idle_phase_returns[i]);
  } else if (fixture_idle_role == 3) fixture_idle_count(&fixture_idle_observer_returns);
}
#if defined(_WIN32)
static void fixture_idle_os_signal(PCONDITION_VARIABLE condition) {
  fixture_idle_signal_entry(condition); WakeAllConditionVariable(condition);
}
static BOOL fixture_idle_os_wait(PCONDITION_VARIABLE condition, PSRWLOCK mutex, DWORD timeout, ULONG flags) {
  fixture_idle_wait_entry(condition,mutex);
  BOOL result = SleepConditionVariableSRW(condition,mutex,timeout,flags);
  DWORD error = GetLastError();
  fixture_idle_wait_return(); SetLastError(error); return result;
}
#else
static int fixture_idle_os_signal(pthread_cond_t* condition) {
  fixture_idle_signal_entry(condition); return pthread_cond_broadcast(condition);
}
static int fixture_idle_os_wait(pthread_cond_t* condition, pthread_mutex_t* mutex) {
  fixture_idle_wait_entry(condition,mutex);
  int result = pthread_cond_wait(condition,mutex);
  fixture_idle_wait_return(); return result;
}
static int fixture_idle_os_timed_wait(pthread_cond_t* condition, pthread_mutex_t* mutex,
                                      const struct timespec* timeout) {
  fixture_idle_wait_entry(condition,mutex);
  int result = pthread_cond_timedwait(condition,mutex,timeout);
  fixture_idle_wait_return(); return result;
}
#if defined(__APPLE__)
static int fixture_idle_os_relative_wait(pthread_cond_t* condition, pthread_mutex_t* mutex,
                                         const struct timespec* timeout) {
  fixture_idle_wait_entry(condition,mutex);
  int result = pthread_cond_timedwait_relative_np(condition,mutex,timeout);
  fixture_idle_wait_return(); return result;
}
#endif
#endif
"#;

const EMPTY_ROUTE_MAIN: &str = r#"
static uint64_t fixture_idle_checked_now(void) {
  uint64_t now = ku_task_driver_now_ms();
  CHECK(now != UINT64_MAX && now < UINT64_MAX-2000u); return now;
}
static unsigned long fixture_idle_remaining(uint64_t deadline) {
  uint64_t now = ku_task_driver_now_ms();
  if (now == UINT64_MAX || now >= deadline) return 0;
  uint64_t remaining = deadline-now;
  return remaining > (uint64_t)ULONG_MAX ? ULONG_MAX : (unsigned long)remaining;
}
static void ku_task_driver_worker(KuTaskDriverWorkerV1* worker) {
  KuTaskDriverV1* driver = worker->driver;
  /* Do not let the test gate bypass init's registration/publication mutex. */
  CHECK(ku_task_driver_lock(driver) == 0);
  CHECK(driver == fixture_idle_driver && driver->workers_created == 2 && worker->index < 2);
  unsigned i = (unsigned)worker->index;
  fixture_idle_role = i+1; fixture_idle_seen[i]++;
#if defined(_WIN32)
  fixture_idle_ids[i] = GetCurrentThreadId();
#else
  fixture_idle_ids[i] = pthread_self();
#endif
  CHECK(ku_task_driver_unlock(driver) == 0);
  if (i == 1) {
    CHECK(ku_test_event_set(&fixture_idle_second_entered));
    CHECK(ku_test_event_wait(&fixture_idle_second_begin,2000));
  }
  fixture_idle_real_worker(worker);
}
static int ku_task_driver_wait_work(KuTaskDriverV1* driver, uint64_t deadline) {
  CHECK(fixture_idle_role == 1 || fixture_idle_role == 2);
  unsigned i = fixture_idle_role-1;
  fixture_idle_kind = 1; fixture_idle_count(&fixture_idle_entries[i]);
  if (fixture_idle_phase == 1 && deadline != UINT64_MAX) fixture_idle_bad_route = 1;
  int result = fixture_idle_real_wait_work(driver,deadline);
  fixture_idle_count(&fixture_idle_returns[i]); fixture_idle_kind = 0; return result;
}
static int ku_task_driver_wait(KuTaskDriverV1* driver, uint64_t deadline) {
  fixture_idle_kind = 2;
  int result = fixture_idle_real_wait(driver,deadline);
  fixture_idle_kind = 0; return result;
}
typedef struct FixtureIdleObserver { KuTaskDriverV1* driver; uint64_t deadline; } FixtureIdleObserver;
static int fixture_idle_observe(void* raw) {
  FixtureIdleObserver* observer = (FixtureIdleObserver*)raw;
  fixture_idle_role = 3;
  return (int)ku_task_driver_wait_idle(observer->driver,observer->deadline);
}
static void fixture_idle_join_observer(KuTestThread* thread, uint64_t deadline) {
  CHECK(ku_test_event_wait(&thread->finished,fixture_idle_remaining(deadline)));
#if defined(_WIN32)
  CHECK(WaitForSingleObject(thread->handle,(DWORD)fixture_idle_remaining(deadline)) == WAIT_OBJECT_0);
  CHECK(CloseHandle(thread->handle)); thread->handle = NULL;
#else
  CHECK(fixture_idle_remaining(deadline)); CHECK(!pthread_join(thread->handle,NULL));
#endif
  CHECK(ku_test_event_destroy(&thread->finished));
}
static void fixture_idle_assert_empty(KuTaskDriverV1* driver) {
  KuTaskDriverSnapshotV1 snapshot = {0};
  CHECK(ku_task_driver_snapshot(driver,&snapshot) == KU_TASK_DRIVER_OK);
  CHECK(snapshot.worker_target == 2 && snapshot.workers_created == 2 && snapshot.workers_waiting == 2
      && !snapshot.workers_exited && !snapshot.workers_joined && !snapshot.closing
      && !snapshot.resident && !snapshot.building && !snapshot.queued && !snapshot.running
      && !snapshot.parked && !snapshot.retiring && !snapshot.terminal_held
      && !snapshot.reserved_bytes && !snapshot.polls && !snapshot.fault && !snapshot.clock_fault);
}
int main(void) {
  CHECK(ku_test_event_init(&fixture_idle_second_entered));
  CHECK(ku_test_event_init(&fixture_idle_second_begin));
  CHECK(ku_test_event_init(&fixture_idle_observer_wait));
  for (size_t i=0;i<2;i++) {
    CHECK(ku_test_event_init(&fixture_idle_first_wait[i]));
    CHECK(ku_test_event_init(&fixture_idle_reparked[i]));
  }
  FixtureDriver runtime = {0};
  runtime.capacity = 1;
  runtime.driver = (KuTaskDriverV1*)calloc(1,sizeof(*runtime.driver));
  runtime.slots = (KuTaskDriverSlotV1*)calloc(1,sizeof(*runtime.slots));
  runtime.ring = (size_t*)calloc(1,sizeof(*runtime.ring));
  CHECK(runtime.driver && runtime.slots && runtime.ring);
  runtime.fixed_bytes = sizeof(*runtime.driver)+sizeof(*runtime.slots)+sizeof(*runtime.ring);
  fixture_idle_driver = runtime.driver;
  const uint64_t startup_deadline = fixture_idle_checked_now()+1000u;
  CHECK(ku_task_driver_init(runtime.driver,sizeof(*runtime.driver),KU_TASK_DRIVER_ABI_VERSION,
      runtime.slots,1,runtime.ring,1,runtime.fixed_bytes,2,startup_deadline) == KU_TASK_DRIVER_OK);
  CHECK(fixture_idle_checked_now() <= startup_deadline);
  CHECK(ku_test_event_wait(&fixture_idle_second_entered,2000));
  CHECK(ku_test_event_wait(&fixture_idle_first_wait[0],2000));
  FixtureIdleObserver observer = { runtime.driver, fixture_idle_checked_now()+2000u };
  KuTestThread observer_thread;
  CHECK(ku_test_thread_start(&observer_thread,fixture_idle_observe,&observer));
  CHECK(ku_test_event_wait(&fixture_idle_observer_wait,fixture_idle_remaining(observer.deadline)));
  /* The event was published under the driver mutex just before the native
   * state wait. Its first actual unlock enables the second worker's park. */
  CHECK(ku_test_event_set(&fixture_idle_second_begin));
  CHECK(ku_test_event_wait(&fixture_idle_first_wait[1],fixture_idle_remaining(observer.deadline)));
  fixture_idle_join_observer(&observer_thread,observer.deadline);
  CHECK(observer_thread.outcome == KU_TASK_DRIVER_OK);
  fixture_idle_assert_empty(runtime.driver);
  CHECK(ku_task_driver_lock(runtime.driver) == 0);
  CHECK(fixture_idle_seen[0] == 1 && fixture_idle_seen[1] == 1
      && fixture_idle_in_wait[0] && fixture_idle_in_wait[1]
      && fixture_idle_observer_entries && fixture_idle_observer_returns);
#if defined(_WIN32)
  CHECK(fixture_idle_ids[0] != fixture_idle_ids[1]);
#else
  CHECK(!pthread_equal(fixture_idle_ids[0],fixture_idle_ids[1]));
#endif
  /* Positive control: wake BOTH genuine work waits, then start the observation
   * phase before releasing the mutex. Their actual returns and re-parking
   * announcements must now occur INSIDE that phase; this broadcast is outside. */
  ku_task_driver_signal_work(runtime.driver);
  fixture_idle_phase = 1;
  CHECK(ku_task_driver_unlock(runtime.driver) == 0);
  const uint64_t observation_deadline = fixture_idle_checked_now()+2000u;
  for (size_t i=0;i<2;i++)
    CHECK(ku_test_event_wait(&fixture_idle_reparked[i],fixture_idle_remaining(observation_deadline)));
  uint64_t sampled_entries[2] = {0}, sampled_returns[2] = {0};
  for (unsigned sample=0;sample<32;sample++) {
    CHECK(fixture_idle_checked_now() < observation_deadline);
    fixture_idle_assert_empty(runtime.driver);
    CHECK(ku_task_driver_wait_idle(runtime.driver,observation_deadline) == KU_TASK_DRIVER_OK);
    CHECK(ku_task_driver_lock(runtime.driver) == 0);
    for (size_t i=0;i<2;i++) {
      CHECK(fixture_idle_in_wait[i] && fixture_idle_phase_entries[i] && fixture_idle_phase_returns[i]);
      CHECK(fixture_idle_entries[i] == fixture_idle_returns[i]+1
          && fixture_idle_entries[i] == fixture_idle_os_entries[i]
          && fixture_idle_returns[i] == fixture_idle_os_returns[i]);
      CHECK(fixture_idle_entries[i] >= sampled_entries[i] && fixture_idle_returns[i] >= sampled_returns[i]);
      sampled_entries[i] = fixture_idle_entries[i]; sampled_returns[i] = fixture_idle_returns[i];
    }
    /* Real state-only pulse. Snapshot and wait_idle themselves do not notify. */
    ku_task_driver_signal(runtime.driver);
    CHECK(ku_task_driver_unlock(runtime.driver) == 0);
  }
  CHECK(ku_task_driver_lock(runtime.driver) == 0);
  const unsigned bad_route = fixture_idle_bad_route;
  const uint64_t work_signals = fixture_idle_work_signals, state_signals = fixture_idle_state_signals;
  const uint64_t park_signals = fixture_idle_park_signals;
  fixture_idle_phase = 2;
  const uint64_t cleanup_deadline = fixture_idle_checked_now()+1000u;
  CHECK(ku_task_driver_unlock(runtime.driver) == 0);
  CHECK(ku_task_driver_shutdown(runtime.driver,cleanup_deadline) == KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_lock(runtime.driver) == 0);
  CHECK(runtime.driver->shutdown_deadline == cleanup_deadline
      && runtime.driver->workers_exited == 2 && !runtime.driver->workers_waiting);
  CHECK(ku_task_driver_unlock(runtime.driver) == 0);
  CHECK(ku_task_driver_join(runtime.driver,cleanup_deadline) == KU_TASK_DRIVER_OK);
  CHECK(runtime.driver->workers_joined == 2);
  for (size_t i=0;i<2;i++) {
    CHECK(runtime.driver->workers[i].state == KU_TASK_DRIVER_WORKER_EXITED
        && runtime.driver->workers[i].joined && runtime.driver->workers[i].closed);
#if defined(_WIN32)
    CHECK(!runtime.driver->workers[i].thread);
#endif
  }
  CHECK(ku_task_driver_destroy(runtime.driver) == KU_TASK_DRIVER_OK);
  CHECK(runtime.driver->initialized == KU_TASK_DRIVER_STORAGE_ZERO && !runtime.driver->sync_resources);
  free(runtime.ring); free(runtime.slots); free(runtime.driver);
  fixture_idle_driver = NULL;
  CHECK(ku_test_event_destroy(&fixture_idle_second_entered));
  CHECK(ku_test_event_destroy(&fixture_idle_second_begin));
  CHECK(ku_test_event_destroy(&fixture_idle_observer_wait));
  for (size_t i=0;i<2;i++) {
    CHECK(ku_test_event_destroy(&fixture_idle_first_wait[i]));
    CHECK(ku_test_event_destroy(&fixture_idle_reparked[i]));
  }
  FixtureLedger ledger = fixture_ledger();
  CHECK(!ledger.allocations && !ledger.bytes && !ledger.overflow);
  CHECK(fixture_idle_checked_now() <= cleanup_deadline);
  /* Strict routing, not an assertion of zero spurious wakes or a CPU limit. */
  CHECK(!bad_route && !work_signals && state_signals >= 32 && park_signals >= 2);
  printf("task-driver-empty-routing-ok waits=%llu,%llu returns=%llu,%llu state=%llu work=%llu\n",
      (unsigned long long)sampled_entries[0],(unsigned long long)sampled_entries[1],
      (unsigned long long)sampled_returns[0],(unsigned long long)sampled_returns[1],
      (unsigned long long)state_signals,(unsigned long long)work_signals);
  return 0;
}
"#;

#[test]
fn native_task_driver_hot_yield_services_marker_before_cancellation() {
    let generated = fixture_source_with_backedge(true);
    let clock_entry = "static uint64_t ku_task_driver_now_ms(void) {";
    let lock_entry = "static int ku_task_driver_lock(KuTaskDriverV1* driver) {";
    let unlock_entry = "static int ku_task_driver_unlock(KuTaskDriverV1* driver) {";
    let poll = "uint32_t outcome = ku_task_control_poll(&execution);";
    let published = "driver->building--;\n    slot->state = KU_TASK_DRIVER_PARKED;\n    ku_task_driver_enqueue(driver, ticket->slot);";
    for unique in [
        "typedef struct KuString {",
        "int main(void) {",
        clock_entry,
        lock_entry,
        unlock_entry,
        poll,
        published,
    ] {
        assert_eq!(generated.matches(unique).count(), 1, "{unique}");
    }
    let instrumentation = format!(
        "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\n{LOCKED_ALLOCATIONS}\n"
    );
    let mut source = generated
        .replacen(
            "typedef struct KuString {",
            &format!("{instrumentation}typedef struct KuString {{"),
            1,
        )
        .replacen("int main(void) {", "static int ku_generated_main(void) {", 1)
        .replacen(
            clock_entry,
            &format!(
                "{HOT_YIELD_LOCK_CALLS_TLS}\n\
                 static void fixture_service_published(const KuTaskDriverTicketV1*, const KuTaskDriverSlotV1*);\n\
                 static void fixture_service_poll_returned(const KuTaskControlLeaseV1*, uint32_t);\n{clock_entry}"
            ),
            1,
        )
        .replacen(
            lock_entry,
            &format!("{lock_entry}\n  fixture_service_count_driver_call(&fixture_service_tls_locks);"),
            1,
        )
        .replacen(
            unlock_entry,
            &format!("{unlock_entry}\n  fixture_service_count_driver_call(&fixture_service_tls_unlocks);"),
            1,
        )
        .replacen(
            poll,
            &format!("{poll}\n    fixture_service_poll_returned(&execution, outcome);"),
            1,
        )
        .replacen(
            published,
            &format!("{published}\n    fixture_service_published(ticket, slot);"),
            1,
        );
    let definitions_end = C_MAIN
        .find("static KuTaskDriverSnapshotV1 fixture_snapshot(")
        .expect("shared driver fixture definitions");
    let callbacks_start = C_MAIN
        .find("static uint64_t fixture_now(")
        .expect("shared driver frame callbacks");
    let callbacks_end = C_MAIN
        .find("static void fixture_wait_disposed(")
        .expect("end of shared driver builder");
    let callbacks = &C_MAIN[callbacks_start..callbacks_end];
    let ops = "fixture_resume, fixture_cleanup, fixture_drop_frame,";
    let increment = "fixture_increment(&witness->resumes);";
    let commit = "return ku_task_driver_commit(ticket, owner, mode, fixture_deadline());";
    for unique in [ops, increment, commit] {
        assert_eq!(callbacks.matches(unique).count(), 1, "{unique}");
    }
    source.push_str(&C_MAIN[..definitions_end]);
    source.push_str(
        "static uint32_t fixture_service_resume(void*);\n\
         static uint32_t fixture_service_cleanup(void*, uint32_t, KuTaskControlV1*);\n\
         static void fixture_service_resume_count(KuAtomicRefcount*);\n\
         static uint64_t fixture_service_build_deadline;\n",
    );
    source.push_str(
        &callbacks
            .replacen(
                ops,
                "fixture_service_resume, fixture_service_cleanup, fixture_drop_frame,",
                1,
            )
            .replacen(
                increment,
                "fixture_service_resume_count(&witness->resumes);",
                1,
            )
            .replacen(
                commit,
                "return ku_task_driver_commit(ticket, owner, mode, fixture_service_build_deadline);",
                1,
            ),
    );
    source.push_str(HOT_YIELD_SERVICE_MAIN);
    let directory = TempDir::new("task-driver-hot-yield-service");
    let c_file = directory.path().join("service.c");
    fs::write(&c_file, source).expect("write real hot-yield service witness");
    let Some(executable) = compile_harness(directory.path(), &c_file, "service") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute the hot-yield service witness"
        );
        return;
    };
    fs::remove_file(&c_file).expect("remove C source before running hot-yield witness");
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("hot-yield service observation and real cancellation must remain bounded");
    assert!(
        output.status.success(),
        "hot-yield service witness failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-driver-hot-yield-cleanup-ok\ntask-driver-hot-yield-lock-calls-ok intervals=3 locks=6 unlocks=6\ntask-driver-hot-yield-service-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const HOT_YIELD_LOCK_CALLS_TLS: &str = r#"
#if defined(_MSC_VER)
#define FIXTURE_SERVICE_THREAD_LOCAL __declspec(thread)
#else
#define FIXTURE_SERVICE_THREAD_LOCAL _Thread_local
#endif
/* Only this fixture's generated C is instrumented. The real driver helpers
 * and OS lock/unlock bodies remain unchanged after the counting statement.
 * Other threads have distinct TLS, so controller and Held calls are excluded. */
static FIXTURE_SERVICE_THREAD_LOCAL int fixture_service_tls_counting;
static FIXTURE_SERVICE_THREAD_LOCAL int fixture_service_tls_overflow;
static FIXTURE_SERVICE_THREAD_LOCAL uint64_t fixture_service_tls_locks;
static FIXTURE_SERVICE_THREAD_LOCAL uint64_t fixture_service_tls_unlocks;
static void fixture_service_count_driver_call(uint64_t* count) {
  if (!fixture_service_tls_counting) return;
  if (*count == UINT64_MAX) fixture_service_tls_overflow = 1;
  else ++*count;
}
"#;

const HOT_YIELD_SERVICE_MAIN: &str = r#"
#if defined(_WIN32)
typedef DWORD FixtureServiceThreadId;
static FixtureServiceThreadId fixture_service_thread_id(void) { return GetCurrentThreadId(); }
static int fixture_service_same_thread(FixtureServiceThreadId a, FixtureServiceThreadId b) { return a == b; }
#else
typedef pthread_t FixtureServiceThreadId;
static FixtureServiceThreadId fixture_service_thread_id(void) { return pthread_self(); }
static int fixture_service_same_thread(FixtureServiceThreadId a, FixtureServiceThreadId b) { return pthread_equal(a,b); }
#endif
enum { SERVICE_HELD, SERVICE_HOT, SERVICE_MARKER, SERVICE_TASKS };
typedef struct FixtureServiceProbe {
  FixtureWitness witness;
  size_t active, maximum_active;
  uint64_t pending_returns;
  int identity_set;
  FixtureServiceThreadId identity;
} FixtureServiceProbe;
static FixtureServiceProbe fixture_service_probes[SERVICE_TASKS];
static FixtureServiceThreadId fixture_service_controller;
static KuTaskControlLeaseV1 fixture_service_hot_observer;
static int fixture_service_counter_fault, fixture_service_warm_sent, fixture_service_marker_sent;
static int fixture_service_held_open, fixture_service_gate_timeout, fixture_service_cancel_started;
static int fixture_service_published_marker, fixture_service_valid;
static uint64_t fixture_service_published_hot, fixture_service_delta;
enum { SERVICE_LOCK_INTERVALS = 3, SERVICE_PAIRS_PER_YIELD = 2 };
static int fixture_service_lock_window_valid;
static uint64_t fixture_service_window_locks, fixture_service_window_unlocks;
static size_t fixture_service_index(FixtureTask* task) {
  for (size_t i = 0; i < SERVICE_TASKS; i++)
    if (task->witness == &fixture_service_probes[i].witness) return i;
  CHECK(false); return 0;
}
/* All probe fields are protected by the existing allocation/probe lock.
 * Saturate and record failure instead of overflow or an early service verdict. */
static void fixture_service_count(uint64_t* count) {
  if (*count == UINT64_MAX) fixture_service_counter_fault = 1;
  else ++*count;
}
static void fixture_service_resume_count(KuAtomicRefcount* count) {
  size_t before = fixture_count(count);
  if (before == SIZE_MAX) {
    fixture_alloc_lock(); fixture_service_counter_fault = 1; fixture_alloc_unlock();
  } else ku_task_control_atomic_store(count, before + 1);
}
static int fixture_service_enter(size_t index, int resume) {
  FixtureServiceProbe* probe = &fixture_service_probes[index];
  fixture_alloc_lock();
  if (probe->active == SIZE_MAX) fixture_service_counter_fault = 1;
  else probe->active++;
  if (probe->active > probe->maximum_active) probe->maximum_active = probe->active;
  int first = resume && !probe->identity_set;
  if (first) { probe->identity = fixture_service_thread_id(); probe->identity_set = 1; }
  fixture_alloc_unlock();
  return first;
}
static void fixture_service_leave(size_t index) {
  fixture_alloc_lock();
  if (!fixture_service_probes[index].active) fixture_service_counter_fault = 1;
  else fixture_service_probes[index].active--;
  fixture_alloc_unlock();
}
static uint32_t fixture_service_resume(void* raw) {
  size_t index = fixture_service_index((FixtureTask*)raw);
  int first = fixture_service_enter(index, 1);
  uint32_t outcome = fixture_resume(raw); /* Actual frame Suspend, actual YIELD intent. */
  CHECK(outcome == KU_TASK_CONTROL_PENDING);
  if (index == SERVICE_HELD && first) {
    CHECK(ku_test_event_set(&fixture_service_probes[index].witness.entered));
    /* Reuse the existing overlap fixture's 5s gate fail-safe; neither the
     * shared 2s observation nor <=1s runtime cleanup budget is extended. */
    int released = ku_test_event_wait(&fixture_service_probes[index].witness.proceed, 5000);
    fixture_alloc_lock();
    fixture_service_held_open = 1;
    if (!released) fixture_service_gate_timeout = 1;
    fixture_alloc_unlock();
  }
  fixture_service_leave(index);
  return outcome;
}
static uint32_t fixture_service_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {
  size_t index = fixture_service_index((FixtureTask*)raw);
  (void)fixture_service_enter(index, 0);
  uint32_t outcome = fixture_cleanup(raw, reason, budget);
  fixture_service_leave(index);
  return outcome;
}
static void fixture_service_published(const KuTaskDriverTicketV1* ticket, const KuTaskDriverSlotV1* slot) {
  /* Called AFTER real enqueue and before releasing the publication mutex.
   * This is an observation of one real ticket, never a second ready index. */
  CHECK(ticket->driver && slot->binding);
  size_t index = fixture_service_index((FixtureTask*)slot->binding->context);
  if (index != SERVICE_MARKER) return;
  fixture_alloc_lock();
  CHECK(!fixture_service_published_marker);
  fixture_service_published_marker = 1;
  fixture_service_published_hot = fixture_service_probes[SERVICE_HOT].pending_returns;
  fixture_alloc_unlock();
}
static void fixture_service_poll_returned(const KuTaskControlLeaseV1* execution, uint32_t outcome) {
  size_t index = fixture_service_index((FixtureTask*)execution->control->context);
  if (outcome != KU_TASK_CONTROL_PENDING) return;
  int notify = 0;
  fixture_alloc_lock();
  FixtureServiceProbe* probe = &fixture_service_probes[index];
  fixture_service_count(&probe->pending_returns);
  if (index == SERVICE_HOT && probe->pending_returns == 1) {
    /* First real R2 return starts the window; its earlier intent is excluded. */
    fixture_service_tls_locks = fixture_service_tls_unlocks = 0;
    fixture_service_tls_overflow = 0;
    fixture_service_tls_counting = 1;
  }
  if (index == SERVICE_HOT && probe->pending_returns >= 4 && !fixture_service_warm_sent) {
    /* Freeze BEFORE the existing event permits Marker publication. The other
     * worker is still Held, so these are three Hot-only yield intervals. */
    fixture_service_lock_window_valid = probe->pending_returns == SERVICE_LOCK_INTERVALS + 1
        && fixture_service_tls_counting && !fixture_service_tls_overflow
        && !fixture_service_held_open && !fixture_service_cancel_started && !fixture_service_published_marker;
    fixture_service_window_locks = fixture_service_tls_locks;
    fixture_service_window_unlocks = fixture_service_tls_unlocks;
    fixture_service_tls_counting = 0;
    fixture_service_warm_sent = 1; notify = 1;
  }
  if (index == SERVICE_MARKER && !fixture_service_marker_sent) {
    fixture_service_marker_sent = 1; notify = 1;
    FixtureServiceProbe* hot = &fixture_service_probes[SERVICE_HOT];
    FixtureServiceProbe* held = &fixture_service_probes[SERVICE_HELD];
    int ordered = fixture_service_published_marker && hot->pending_returns >= fixture_service_published_hot;
    fixture_service_delta = ordered ? hot->pending_returns - fixture_service_published_hot : UINT64_MAX;
    /* This real lease was retained before Marker publication; the controller
     * cannot release it or cancel Hot before this marker observation. */
    int live = fixture_service_hot_observer.control
        && ku_task_control_atomic_load(&fixture_service_hot_observer.control->phase) == KU_TASK_CONTROL_LIVE;
    fixture_service_valid = ordered && fixture_service_warm_sent && hot->pending_returns >= 4
        && !fixture_service_counter_fault && !fixture_service_cancel_started
        && !fixture_service_held_open && !fixture_service_gate_timeout && held->active == 1
        && held->identity_set && hot->identity_set && probe->identity_set && live
        && fixture_service_same_thread(probe->identity, hot->identity)
        && !fixture_service_same_thread(probe->identity, held->identity)
        && !fixture_service_same_thread(probe->identity, fixture_service_controller)
        && fixture_service_delta <= 1;
  }
  fixture_alloc_unlock();
  /* First transition only: no per-poll event amplification. Never wait while
   * holding the probe lock, and never call a driver API under that lock. */
  if (notify) CHECK(ku_test_event_set(&fixture_service_probes[index].witness.entered));
}
static uint64_t fixture_service_real_deadline(uint64_t span) {
  uint64_t now = ku_test_real_now_ms();
  CHECK(now && span && now < UINT64_MAX - span);
  return now + span;
}
static unsigned long fixture_service_remaining(uint64_t deadline) {
  uint64_t now = ku_test_real_now_ms(); CHECK(now);
  if (now >= deadline) return 0;
  CHECK(deadline - now <= ULONG_MAX);
  return (unsigned long)(deadline - now);
}
static void fixture_service_wait_acks(FixtureDriver* runtime, KuTaskDriverCleanupReceiptV1* receipts, uint64_t deadline) {
  CHECK(ku_task_driver_lock(runtime->driver) == 0);
  for (;;) {
    int ready = 1;
    for (size_t i = 0; i < SERVICE_TASKS; i++) {
      uint32_t ack = ku_task_driver_cleanup_receipt_status_locked(
          runtime->driver, receipts[i].slot, receipts[i].generation);
      CHECK(ack == KU_TASK_DRIVER_CLEANUP_ACK || ack == KU_TASK_DRIVER_PENDING);
      if (ack != KU_TASK_DRIVER_CLEANUP_ACK) ready = 0;
    }
    if (ready) break;
    CHECK(!runtime->driver->fault && fixture_service_remaining(deadline));
    int waited = ku_task_driver_wait(runtime->driver, deadline);
    CHECK(waited == 0 || waited == 1);
  }
  CHECK(ku_task_driver_unlock(runtime->driver) == 0);
}
int main(void) {
  CHECK(DRIVER_ABI == 7u && FRAME_ABI == 4u && CONTROL_ABI == 2u);
  fixture_service_controller = fixture_service_thread_id();
  for (size_t i = 0; i < SERVICE_TASKS; i++) fixture_witness_init(&fixture_service_probes[i].witness);
  FixtureDriver runtime = {0};
  runtime.capacity = SERVICE_TASKS;
  runtime.driver = (KuTaskDriverV1*)calloc(1, sizeof(*runtime.driver));
  runtime.slots = (KuTaskDriverSlotV1*)calloc(SERVICE_TASKS, sizeof(*runtime.slots));
  runtime.ring = (size_t*)calloc(SERVICE_TASKS, sizeof(*runtime.ring));
  CHECK(runtime.driver && runtime.slots && runtime.ring);
  runtime.fixed_bytes = sizeof(*runtime.driver) + SERVICE_TASKS * (sizeof(*runtime.slots) + sizeof(*runtime.ring));
  CHECK(fixture_task_bytes() <= (SIZE_MAX - runtime.fixed_bytes) / SERVICE_TASKS);
  uint64_t startup_deadline = fixture_driver_one_second();
  fixture_service_build_deadline = startup_deadline;
  CHECK(ku_task_driver_init(runtime.driver, sizeof(*runtime.driver), DRIVER_ABI,
      runtime.slots, SERVICE_TASKS, runtime.ring, SERVICE_TASKS,
      runtime.fixed_bytes + SERVICE_TASKS * fixture_task_bytes(), 2, startup_deadline) == KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_wait_idle(runtime.driver, startup_deadline) == KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_lock(runtime.driver) == 0);
  CHECK(runtime.driver->workers_created == 2 && runtime.driver->workers_waiting == 2);
  CHECK(!runtime.driver->resident && !runtime.driver->running && !runtime.driver->queued);
  CHECK(ku_task_driver_unlock(runtime.driver) == 0);
  KuTaskControlOwnerV1 owners[SERVICE_TASKS] = {0};
  KuTaskDriverTicketV1 tickets[SERVICE_TASKS] = {0};
  KuTaskDriverCleanupReceiptV1 receipts[SERVICE_TASKS] = {0};
  int observed[SERVICE_TASKS] = {0};
  const uint64_t observation_deadline = fixture_service_real_deadline(2000);
  for (size_t i = 0; i < SERVICE_TASKS; i++) {
    FixtureInputs inputs = fixture_inputs();
    CHECK(fixture_build(&runtime, &inputs, &fixture_service_probes[i].witness,
        &owners[i], &tickets[i], FIXTURE_NORMAL, false, false, false, false) == KU_TASK_DRIVER_OK);
    CHECK(!inputs.result.value.ptr && !inputs.cleanup.ptr);
    fixture_inputs_drop(&inputs);
    observed[i] = ku_test_event_wait(&fixture_service_probes[i].witness.entered,
        fixture_service_remaining(observation_deadline));
    if (i == SERVICE_HOT)
      CHECK(ku_task_control_lease_retain(&owners[i].lease, &fixture_service_hot_observer) == KU_TASK_CONTROL_OK);
  }
  /* Marker observation, not a finite hot loop or a sleep, triggers cleanup.
   * Missing service follows the same genuine reclamation path before verdict. */
  const uint64_t cleanup_deadline = fixture_service_real_deadline(1000);
  fixture_alloc_lock();
  fixture_service_cancel_started = 1;
  int served = observed[0] && observed[1] && observed[2] && fixture_service_valid;
  uint64_t delta = fixture_service_delta;
  fixture_alloc_unlock();
  for (size_t i = 0; i < SERVICE_TASKS; i++) {
    CHECK(ku_task_driver_request_cancel(&tickets[i], &owners[i].lease,
        KU_TASK_CONTROL_CANCELLED, cleanup_deadline) == KU_TASK_CONTROL_OK);
    CHECK(ku_task_driver_owner_drop_receipt(&tickets[i], &owners[i], cleanup_deadline, &receipts[i]) == KU_TASK_DRIVER_OK);
    CHECK(!owners[i].lease.control);
  }
  CHECK(ku_test_event_set(&fixture_service_probes[SERVICE_HELD].witness.proceed));
  fixture_service_wait_acks(&runtime, receipts, cleanup_deadline);
  CHECK(ku_task_control_status(&fixture_service_hot_observer) == KU_TASK_CONTROL_CANCELLED);
  CHECK(fixture_service_hot_observer.control->frame_destroyed
      && !ku_task_control_atomic_load(&fixture_service_hot_observer.control->lifecycle_pin));
  CHECK(ku_task_control_lease_release(&fixture_service_hot_observer) == KU_TASK_CONTROL_OK);
  CHECK(!fixture_service_hot_observer.control);
  for (size_t i = 0; i < SERVICE_TASKS; i++) {
    CHECK(ku_test_event_wait(&fixture_service_probes[i].witness.disposed, fixture_service_remaining(cleanup_deadline)));
    CHECK(ku_task_driver_cleanup_receipt_read(&receipts[i]) == KU_TASK_DRIVER_CLEANUP_ACK);
  }
  CHECK(ku_task_driver_shutdown(runtime.driver, cleanup_deadline) == KU_TASK_DRIVER_OK);
  CHECK(runtime.driver->shutdown_deadline == cleanup_deadline);
  KuTaskDriverSnapshotV1 finished = {0};
  CHECK(ku_task_driver_snapshot(runtime.driver, &finished) == KU_TASK_DRIVER_OK);
  CHECK(!finished.fault && !finished.clock_fault && !finished.resident && !finished.reserved_bytes);
  CHECK(!finished.building && !finished.running && !finished.queued && !finished.parked);
  CHECK(!finished.retiring && !finished.terminal_held && !finished.workers_waiting);
  CHECK(finished.workers_created == 2 && finished.workers_exited == 2);
  CHECK(ku_task_driver_join(runtime.driver, cleanup_deadline) == KU_TASK_DRIVER_OK);
  CHECK(runtime.driver->workers_joined == 2);
  for (size_t i = 0; i < 2; i++) CHECK(runtime.driver->workers[i].joined && runtime.driver->workers[i].closed);
  CHECK(ku_task_driver_destroy(runtime.driver) == KU_TASK_DRIVER_OK);
  free(runtime.ring); free(runtime.slots); free(runtime.driver);
  fixture_alloc_lock();
  int serialized = !fixture_service_counter_fault && !fixture_service_gate_timeout;
  uint64_t lock_calls = fixture_service_window_locks, unlock_calls = fixture_service_window_unlocks;
  uint64_t expected_calls = SERVICE_LOCK_INTERVALS * SERVICE_PAIRS_PER_YIELD;
  int lock_window_valid = fixture_service_lock_window_valid;
  int lock_calls_valid = lock_window_valid
      && lock_calls == expected_calls && unlock_calls == expected_calls;
  for (size_t i = 0; i < SERVICE_TASKS; i++)
    serialized = serialized && fixture_service_probes[i].maximum_active == 1 && !fixture_service_probes[i].active;
  fixture_alloc_unlock();
  for (size_t i = 0; i < SERVICE_TASKS; i++) {
    FixtureWitness* witness = &fixture_service_probes[i].witness;
    CHECK(fixture_count(&witness->cleanups) == 1 && fixture_count(&witness->frame_drops) == 1);
    CHECK(fixture_count(&witness->disposes) == 1 && !fixture_count(&witness->takes) && !fixture_count(&witness->payload_drops));
    CHECK(witness->cleanup_deadline == cleanup_deadline);
    fixture_witness_finish(witness);
  }
  FixtureLedger ledger = fixture_ledger();
  CHECK(!ledger.overflow && !ledger.allocations && !ledger.bytes);
  CHECK(ku_test_real_now_ms() <= cleanup_deadline);
  puts("task-driver-hot-yield-cleanup-ok");
  /* delta<=1 describes today's private shared FIFO dispatch policy only.
   * It is not public FIFO, a wall-clock fairness promise or a performance gate. */
  if (!served || !serialized || !lock_calls_valid) {
    fprintf(stderr, "hot-yield service failed after actual cleanup: served=%d serialized=%d delta=%llu lock_window=%d intervals=%u locks=%llu unlocks=%llu expected=%llu\n",
        served, serialized, (unsigned long long)delta, lock_window_valid, (unsigned)SERVICE_LOCK_INTERVALS,
        (unsigned long long)lock_calls, (unsigned long long)unlock_calls, (unsigned long long)expected_calls);
    return 1;
  }
  /* Mechanism only: actual helper calls, not timing, contention or throughput. */
  printf("task-driver-hot-yield-lock-calls-ok intervals=%u locks=%llu unlocks=%llu\n",
      (unsigned)SERVICE_LOCK_INTERVALS, (unsigned long long)lock_calls, (unsigned long long)unlock_calls);
  puts("task-driver-hot-yield-service-ok");
  return 0;
}
"#;

#[test]
fn native_task_driver_two_workers_clock_fault_recovers_late_owner_and_builder() {
    let generated = fixture_source();
    let clock_entry = "static uint64_t ku_task_driver_now_ms(void) {";
    let worker_entry = "static void ku_task_driver_worker(KuTaskDriverWorkerV1* worker) {";
    for unique in [
        "typedef struct KuString {",
        "int main(void) {",
        clock_entry,
        worker_entry,
    ] {
        assert_eq!(generated.matches(unique).count(), 1, "{unique}");
    }
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
            clock_entry,
            &format!(
                "{TWO_WORKER_CLOCK_FAULT_CLOCK}\nstatic uint64_t fixture_cf_real_now(void) {{"
            ),
            1,
        )
        .replacen(
            worker_entry,
            &format!("{worker_entry}\n  fixture_cf_lane = worker->index + 1u;"),
            1,
        );
    // Reuse the real Owned frame, callbacks, builder and locked allocation
    // ledger. Only observe callback identity/reason and select the clock fault.
    let definitions_end = C_MAIN
        .find("static KuTaskDriverSnapshotV1 fixture_snapshot(")
        .expect("shared driver fixture definitions");
    let callbacks_start = C_MAIN
        .find("static uint64_t fixture_now(")
        .expect("shared driver frame callbacks");
    let callbacks_end = C_MAIN
        .find("static void fixture_wait_disposed(")
        .expect("end of shared driver builder");
    let callbacks = &C_MAIN[callbacks_start..callbacks_end];
    let ops_entry = "fixture_resume, fixture_cleanup, fixture_drop_frame,";
    let commit_entry = "return ku_task_driver_commit(ticket, owner, mode, fixture_deadline());";
    assert_eq!(callbacks.matches(ops_entry).count(), 1);
    assert_eq!(callbacks.matches(commit_entry).count(), 1);
    source.push_str(&C_MAIN[..definitions_end]);
    source.push_str(
        "static uint32_t fixture_cf_resume(void* raw);\n\
         static uint32_t fixture_cf_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget);\n\
         static uint64_t fixture_cf_build_deadline;\n",
    );
    source.push_str(
        &callbacks
            .replacen(
                ops_entry,
                "fixture_cf_resume, fixture_cf_cleanup, fixture_drop_frame,",
                1,
            )
            .replacen(
                commit_entry,
                "return ku_task_driver_commit(ticket, owner, mode, fixture_cf_build_deadline);",
                1,
            ),
    );
    source.push_str(TWO_WORKER_CLOCK_FAULT_MAIN);
    let directory = TempDir::new("task-driver-two-worker-clock-fault");
    let c_file = directory.path().join("clock_fault.c");
    fs::write(&c_file, source).expect("write real two-worker clock-fault witness");
    let Some(executable) = compile_harness(directory.path(), &c_file, "clock_fault") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute the real two-worker clock-fault witness"
        );
        return;
    };
    fs::remove_file(&c_file).expect("remove C source before running clock-fault witness");
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("late clock-fault cleanup observation must remain bounded");
    assert!(
        output.status.success(),
        "two-worker clock-fault witness failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-driver-two-worker-clock-fault-late-recovery-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const TWO_WORKER_CLOCK_FAULT_CLOCK: &str = r#"
static KU_THREAD_LOCAL size_t fixture_cf_lane;
static KuAtomicRefcount fixture_cf_bad_lane, fixture_cf_bad_read;
static uint64_t fixture_cf_real_now(void);
static uint64_t ku_task_driver_now_ms(void) {
  /* Only an actual generated worker gets a nonzero TLS lane. The injected
   * failure is an actual clock result, never a written driver fault/phase.
   * Controller time and test-event clocks remain real and uninjected. */
  if (fixture_cf_lane && fixture_cf_lane == ku_task_control_atomic_load(&fixture_cf_bad_lane)) {
    ku_task_control_atomic_store(&fixture_cf_bad_read, 1);
    return UINT64_MAX;
  }
  return fixture_cf_real_now();
}
"#;

const TWO_WORKER_CLOCK_FAULT_MAIN: &str = r#"
typedef struct FixtureCfProbe {
  FixtureWitness witness;
  size_t resume_lane, cleanup_lane;
  uint32_t cleanup_reason;
} FixtureCfProbe;
static FixtureCfProbe fixture_cf_probes[2];
static FixtureCfProbe* fixture_cf_probe(FixtureTask* task) {
  for (size_t i = 0; i < 2; i++)
    if (task->witness == &fixture_cf_probes[i].witness) return &fixture_cf_probes[i];
  CHECK(false); return NULL;
}
static uint32_t fixture_cf_resume(void* raw) {
  FixtureCfProbe* probe = fixture_cf_probe((FixtureTask*)raw);
  CHECK(fixture_cf_lane >= 1 && fixture_cf_lane <= 2 && !probe->resume_lane);
  probe->resume_lane = fixture_cf_lane;
  return fixture_resume(raw);
}
static uint32_t fixture_cf_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {
  FixtureCfProbe* probe = fixture_cf_probe((FixtureTask*)raw);
  CHECK(fixture_cf_lane >= 1 && fixture_cf_lane <= 2 && !probe->cleanup_lane);
  probe->cleanup_lane = fixture_cf_lane;
  probe->cleanup_reason = reason;
  return fixture_cleanup(raw, reason, budget);
}
static uint64_t fixture_cf_real_deadline(void) {
  uint64_t now = ku_test_real_now_ms();
  CHECK(now && now < UINT64_MAX - 2000u);
  return now + 2000u;
}
static unsigned long fixture_cf_remaining(uint64_t deadline) {
  uint64_t now = ku_test_real_now_ms();
  CHECK(now && now < deadline && deadline - now <= ULONG_MAX);
  return (unsigned long)(deadline - now);
}
/* Real, independent state observation after a poisoned runtime clock. All
 * callers own the driver's actual mutex; this never changes runtime D=0. */
static void fixture_cf_state_wait_real(KuTaskDriverV1* driver, uint64_t deadline) {
  uint64_t now = ku_test_real_now_ms();
  CHECK(now && now < deadline);
#if defined(_WIN32)
  uint64_t remaining = deadline - now;
  CHECK(remaining < (uint64_t)INFINITE);
  if (!SleepConditionVariableSRW(&driver->condition, &driver->mutex, (DWORD)remaining, 0))
    CHECK(GetLastError() == ERROR_TIMEOUT);
#else
  struct timespec timeout;
#if defined(__APPLE__)
  uint64_t remaining = deadline - now;
  timeout.tv_sec = (time_t)(remaining / 1000u);
  timeout.tv_nsec = (long)((remaining % 1000u) * 1000000u);
  int waited = pthread_cond_timedwait_relative_np(&driver->condition, &driver->mutex, &timeout);
#else
  timeout.tv_sec = (time_t)(deadline / 1000u);
  CHECK((uint64_t)timeout.tv_sec == deadline / 1000u);
  timeout.tv_nsec = (long)((deadline % 1000u) * 1000000u);
  int waited = pthread_cond_timedwait(&driver->condition, &driver->mutex, &timeout);
#endif
  CHECK(waited == 0 || waited == ETIMEDOUT);
#endif
}
static void fixture_cf_assert_fault_locked(KuTaskDriverV1* driver) {
  CHECK(driver->fault == KU_TASK_DRIVER_INTERNAL && driver->clock_fault && driver->closing);
  CHECK(driver->shutdown_deadline == 0 && driver->worker_target == 2 && driver->workers_created == 2);
}
int main(void) {
  CHECK(DRIVER_ABI == 7u && FRAME_ABI == 4u && CONTROL_ABI == 2u);
  ku_task_control_atomic_init(&fixture_cf_bad_lane, 0);
  ku_task_control_atomic_init(&fixture_cf_bad_read, 0);
  FixtureDriver runtime = {0};
  runtime.capacity = 2;
  runtime.driver = (KuTaskDriverV1*)calloc(1, sizeof(*runtime.driver));
  runtime.slots = (KuTaskDriverSlotV1*)calloc(2, sizeof(*runtime.slots));
  runtime.ring = (size_t*)calloc(2, sizeof(*runtime.ring));
  CHECK(runtime.driver && runtime.slots && runtime.ring);
  runtime.fixed_bytes = sizeof(*runtime.driver) + 2 * (sizeof(*runtime.slots) + sizeof(*runtime.ring));
  CHECK(fixture_task_bytes() <= (SIZE_MAX - runtime.fixed_bytes) / 2);
  uint64_t startup_deadline = fixture_driver_one_second();
  fixture_cf_build_deadline = startup_deadline;
  CHECK(ku_task_driver_init(runtime.driver, sizeof(*runtime.driver), DRIVER_ABI,
      runtime.slots, 2, runtime.ring, 2, runtime.fixed_bytes + 2 * fixture_task_bytes(),
      2, startup_deadline) == KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_wait_idle(runtime.driver, startup_deadline) == KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_lock(runtime.driver) == 0);
  CHECK(runtime.driver->workers_waiting == 2 && !runtime.driver->workers_exited);
  CHECK(!runtime.driver->resident && !runtime.driver->queued && !runtime.driver->running);
  CHECK(ku_task_driver_unlock(runtime.driver) == 0);
  for (size_t i = 0; i < 2; i++) fixture_witness_init(&fixture_cf_probes[i].witness);
  KuTaskControlOwnerV1 owners[2] = {0};
  KuTaskControlLeaseV1 observers[2] = {0};
  KuTaskDriverTicketV1 tickets[2] = {0};
  KuTaskDriverCleanupReceiptV1 receipts[2] = {0};
  FixtureInputs inputs = fixture_inputs();
  CHECK(fixture_build(&runtime, &inputs, &fixture_cf_probes[0].witness, &owners[0], &tickets[0],
      FIXTURE_NORMAL, true, true, false, false) == KU_TASK_DRIVER_OK);
  fixture_inputs_drop(&inputs);
  CHECK(ku_test_event_wait(&fixture_cf_probes[0].witness.entered, 2000));
  size_t active_lane = fixture_cf_probes[0].resume_lane;
  CHECK(active_lane >= 1 && active_lane <= 2);
  size_t peer_lane = 3u - active_lane;
  CHECK(ku_task_driver_reserve(runtime.driver, fixture_task_bytes(), &tickets[1]) == KU_TASK_DRIVER_OK);
  uint64_t first_cleanup_deadline = fixture_driver_one_second();
  CHECK(ku_task_driver_request_cancel(&tickets[0], &owners[0].lease,
      KU_TASK_CONTROL_TIMED_OUT, first_cleanup_deadline) == KU_TASK_CONTROL_OK);
  CHECK(ku_task_control_atomic_load(&owners[0].lease.control->phase) == KU_TASK_CONTROL_REQUESTED_TIMEOUT);
  /* One fixed real observation window. It is NOT a new cleanup budget. A is
   * held inside the real Pending callback, while the peer is truly parked. */
  const uint64_t observation_deadline = fixture_cf_real_deadline();
  CHECK(ku_task_driver_lock(runtime.driver) == 0);
  while (runtime.driver->workers_waiting != 1)
    fixture_cf_state_wait_real(runtime.driver, observation_deadline);
  CHECK(!runtime.driver->fault && !runtime.driver->closing && !runtime.driver->clock_fault);
  CHECK(runtime.driver->workers[active_lane - 1].state == KU_TASK_DRIVER_WORKER_ACTIVE);
  CHECK(runtime.driver->workers[peer_lane - 1].state == KU_TASK_DRIVER_WORKER_WAITING);
  CHECK(runtime.driver->resident == 2 && runtime.driver->building == 1);
  CHECK(runtime.driver->running == 1 && !runtime.driver->queued && !runtime.driver->polls);
  CHECK(runtime.slots[tickets[1].slot].state == KU_TASK_DRIVER_BUILDING);
  ku_task_control_atomic_store(&fixture_cf_bad_lane, peer_lane);
  ku_task_driver_signal_work(runtime.driver); /* The native peer wait must really return. */
  while (!runtime.driver->clock_fault || runtime.driver->workers_waiting != 1)
    fixture_cf_state_wait_real(runtime.driver, observation_deadline);
  fixture_cf_assert_fault_locked(runtime.driver);
  CHECK(ku_task_control_atomic_load(&fixture_cf_bad_read) == 1);
  CHECK(runtime.driver->running == 1 && !runtime.driver->queued && runtime.driver->building == 1);
  CHECK(runtime.driver->resident == 2 && runtime.driver->reserved_bytes == 2 * fixture_task_bytes());
  CHECK(runtime.slots[tickets[1].slot].state == KU_TASK_DRIVER_BUILDING);
  CHECK(ku_task_driver_unlock(runtime.driver) == 0);
  CHECK(ku_task_control_atomic_load(&owners[0].lease.control->phase) == KU_TASK_CONTROL_REQUESTED_TIMEOUT);
  CHECK(ku_task_control_cleanup_deadline(owners[0].lease.control) == 0);
  CHECK(ku_task_driver_wait_idle(runtime.driver, 0) == KU_TASK_DRIVER_INTERNAL);
  CHECK(ku_task_driver_shutdown(runtime.driver, 0) == KU_TASK_DRIVER_INTERNAL);
  KuTaskDriverTicketV1 rejected = {0};
  CHECK(ku_task_driver_reserve(runtime.driver, 1, &rejected) == KU_TASK_DRIVER_CLOSED);
  CHECK(!rejected.driver);

  /* Finish only the reservation acquired BEFORE closing. Normal START retains
   * B's user owner; it must enter cleanup without resuming the real frame. */
  fixture_cf_build_deadline = 0;
  inputs = fixture_inputs();
  CHECK(fixture_build(&runtime, &inputs, &fixture_cf_probes[1].witness, &owners[1], &tickets[1],
      FIXTURE_NORMAL, true, false, false, true) == KU_TASK_DRIVER_OK);
  fixture_inputs_drop(&inputs);
  CHECK(ku_test_event_wait(&fixture_cf_probes[1].witness.entered, fixture_cf_remaining(observation_deadline)));
  CHECK(fixture_cf_probes[1].cleanup_lane == peer_lane);
  CHECK(fixture_cf_probes[1].cleanup_reason == KU_TASK_CONTROL_CANCELLED);
  CHECK(!fixture_count(&fixture_cf_probes[1].witness.resumes));
  CHECK(ku_task_control_atomic_load(&owners[1].lease.control->phase) == KU_TASK_CONTROL_REQUESTED_CANCEL);
  CHECK(ku_task_control_cleanup_deadline(owners[1].lease.control) == 0);
  for (size_t i = 0; i < 2; i++) {
    /* Both execution callbacks are gated: no concurrent pin/reference release,
     * so retain has no artificial retry or invented reference-count write. */
    CHECK(ku_task_control_lease_retain(&owners[i].lease, &observers[i]) == KU_TASK_CONTROL_OK);
    CHECK(ku_task_driver_owner_drop_receipt(&tickets[i], &owners[i], 0, &receipts[i]) == KU_TASK_DRIVER_OK);
    CHECK(!owners[i].lease.control);
    CHECK(ku_task_driver_cleanup_receipt_read(&receipts[i]) == KU_TASK_DRIVER_PENDING);
  }
  CHECK(ku_test_event_set(&fixture_cf_probes[0].witness.proceed));
  CHECK(ku_test_event_set(&fixture_cf_probes[1].witness.proceed));
  CHECK(ku_task_driver_lock(runtime.driver) == 0);
  while (runtime.driver->workers_waiting != 2 || runtime.driver->queued || runtime.driver->running
      || runtime.slots[tickets[0].slot].cleanup_acked_generation != tickets[0].generation
      || runtime.slots[tickets[1].slot].cleanup_acked_generation != tickets[1].generation)
    fixture_cf_state_wait_real(runtime.driver, observation_deadline);
  fixture_cf_assert_fault_locked(runtime.driver);
  CHECK(!runtime.driver->building && runtime.driver->resident == 2 && !runtime.driver->workers_exited);
  CHECK(runtime.driver->reserved_bytes == 2 * fixture_task_bytes() && runtime.driver->polls == 4);
  for (size_t i = 0; i < 2; i++) {
    CHECK(runtime.slots[tickets[i].slot].state == KU_TASK_DRIVER_RETIRING);
    CHECK(!runtime.slots[tickets[i].slot].binding && !runtime.slots[tickets[i].slot].wrapper_active);
  }
  CHECK(ku_task_driver_unlock(runtime.driver) == 0);
  for (size_t i = 0; i < 2; i++) {
    FixtureCfProbe* probe = &fixture_cf_probes[i];
    uint32_t expected = i == 0 ? KU_TASK_CONTROL_TIMED_OUT : KU_TASK_CONTROL_CANCELLED;
    CHECK(ku_task_driver_cleanup_receipt_read(&receipts[i]) == KU_TASK_DRIVER_CLEANUP_ACK);
    CHECK(ku_task_control_status(&observers[i]) == expected && probe->cleanup_reason == expected);
    CHECK(observers[i].control->frame_destroyed && !ku_task_control_atomic_load(&observers[i].control->lifecycle_pin));
    CHECK(ku_task_control_atomic_load(&observers[i].control->payload) == KU_TASK_CONTROL_PAYLOAD_EMPTY);
    CHECK(fixture_count(&probe->witness.resumes) == (i == 0 ? 1u : 0u));
    CHECK(fixture_count(&probe->witness.cleanups) == 1 && fixture_count(&probe->witness.frame_drops) == 1);
    CHECK(!fixture_count(&probe->witness.payload_drops) && !fixture_count(&probe->witness.takes));
    CHECK(!fixture_count(&probe->witness.disposes) && probe->witness.cleanup_deadline == 0);
  }
  CHECK(fixture_cf_probes[0].cleanup_lane == active_lane && fixture_cf_probes[1].cleanup_lane == peer_lane);
  /* The actual frame and both Owned 7-byte inputs per task are gone. Retained
   * observers keep exactly two contexts and their real admission charges. */
  FixtureLedger held = fixture_ledger();
  CHECK(!held.overflow && held.allocations == 5 && held.bytes == runtime.fixed_bytes + 2 * sizeof(FixtureTask));
  for (size_t i = 0; i < 2; i++) {
    CHECK(ku_task_control_lease_release(&observers[i]) == KU_TASK_CONTROL_OK && !observers[i].control);
    CHECK(ku_test_event_wait(&fixture_cf_probes[i].witness.disposed, fixture_cf_remaining(observation_deadline)));
    CHECK(fixture_count(&fixture_cf_probes[i].witness.disposes) == 1);
    CHECK(ku_task_driver_cleanup_receipt_read(&receipts[i]) == KU_TASK_DRIVER_CLEANUP_ACK);
  }
  CHECK(ku_task_driver_lock(runtime.driver) == 0);
  while (runtime.driver->workers_exited != 2)
    fixture_cf_state_wait_real(runtime.driver, observation_deadline);
  fixture_cf_assert_fault_locked(runtime.driver);
  CHECK(!runtime.driver->workers_waiting && !runtime.driver->workers_joined);
  CHECK(!runtime.driver->resident && !runtime.driver->building && !runtime.driver->reserved_bytes);
  CHECK(!runtime.driver->queued && !runtime.driver->running && runtime.driver->polls == 4);
  for (size_t i = 0; i < 2; i++) {
    CHECK(runtime.driver->workers[i].state == KU_TASK_DRIVER_WORKER_EXITED);
    CHECK(!runtime.driver->workers[i].joined && !runtime.driver->workers[i].closed);
    CHECK(runtime.slots[i].state == KU_TASK_DRIVER_FREE);
  }
  CHECK(ku_task_driver_unlock(runtime.driver) == 0);
  CHECK(ku_task_control_atomic_load(&fixture_cf_bad_lane) == peer_lane);
  CHECK(ku_task_driver_wait_idle(runtime.driver, 0) == KU_TASK_DRIVER_INTERNAL);
  CHECK(ku_task_driver_shutdown(runtime.driver, 0) == KU_TASK_DRIVER_INTERNAL);
  CHECK(ku_task_driver_destroy(runtime.driver) == KU_TASK_DRIVER_PENDING);
#if defined(_WIN32)
  /* EXITED is the last driver-storage access, not OS-return readiness. Use the
   * SAME real observation D only after actual all-EXITED/no-Task, never close
   * or fake joined bits here. Resource-only join still receives original D=0. */
  for (size_t i = 0; i < 2; i++)
    CHECK(WaitForSingleObject(runtime.driver->workers[i].thread,
        (DWORD)fixture_cf_remaining(observation_deadline)) == WAIT_OBJECT_0);
#endif
  CHECK(ku_task_driver_join(runtime.driver, 0) == KU_TASK_DRIVER_OK);
  CHECK(runtime.driver->workers_joined == 2 && runtime.driver->shutdown_deadline == 0);
  CHECK(runtime.driver->fault == KU_TASK_DRIVER_INTERNAL && runtime.driver->clock_fault);
  for (size_t i = 0; i < 2; i++)
    CHECK(runtime.driver->workers[i].joined && runtime.driver->workers[i].closed);
  CHECK(ku_task_driver_destroy(runtime.driver) == KU_TASK_DRIVER_OK);
  free(runtime.ring); free(runtime.slots); free(runtime.driver);
  for (size_t i = 0; i < 2; i++) fixture_witness_finish(&fixture_cf_probes[i].witness);
  FixtureLedger final = fixture_ledger();
  CHECK(!final.overflow && !final.allocations && !final.bytes);
  /* POSIX join has no portable hard OS-return limit: this final observation
   * and the process watchdog reject delay, but do not promise bounded join. */
  CHECK(ku_test_real_now_ms() <= observation_deadline);
  puts("task-driver-two-worker-clock-fault-late-recovery-ok");
  return 0;
}
"#;

#[test]
fn native_task_driver_shutdown_numeric_range_keeps_a_finite_original_budget() {
    let generated = fixture_source();
    let shutdown_start = generated
        .find("static uint32_t ku_task_driver_shutdown(")
        .expect("actual generated shutdown");
    let root_lines = generated[shutdown_start..]
        .lines()
        .filter(|line| line.trim_start().starts_with("uint64_t root = "))
        .collect::<Vec<_>>();
    assert_eq!(
        root_lines.len(),
        1,
        "extract actual shutdown root arithmetic"
    );
    let instrumentation = format!(
        "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\n{LOCKED_ALLOCATIONS}\n"
    );
    let mut source = generated.clone();
    for (anchor, replacement) in [
        (
            "typedef struct KuString {",
            format!("{instrumentation}typedef struct KuString {{"),
        ),
        ("int main(void) {", "static int ku_generated_main(void) {".to_owned()),
        (
            "static uint64_t ku_task_driver_now_ms(void) {",
            "static uint64_t ku_task_driver_now_ms(void);\nstatic void fixture_numeric_os_path(uint64_t);\nstatic uint64_t fixture_numeric_real_now(void) {".to_owned(),
        ),
        (
            "static int ku_task_driver_wait(KuTaskDriverV1* driver, uint64_t deadline) {",
            "static int ku_task_driver_wait(KuTaskDriverV1*,uint64_t);\nstatic int fixture_numeric_real_wait(KuTaskDriverV1* driver, uint64_t deadline) {".to_owned(),
        ),
        (
            "  if (deadline != UINT64_MAX && now >= deadline) return 1;",
            "  if (deadline != UINT64_MAX && now >= deadline) return 1;\n  fixture_numeric_os_path(deadline);".to_owned(),
        ),
    ] {
        assert_eq!(source.matches(anchor).count(), 1, "numeric-boundary hook");
        source = source.replacen(anchor, &replacement, 1);
    }
    let definitions_end = C_MAIN
        .find("static KuTaskDriverSnapshotV1 fixture_snapshot(")
        .unwrap();
    source.push_str(&C_MAIN[..definitions_end]);
    source.push_str(SHUTDOWN_NUMERIC_PROBE);
    // Reuse real lifecycle helpers, without the unrelated A/B observer callback.
    let timing_start = SHUTDOWN_TIGHTENING_MAIN
        .find("static uint64_t fixture_shutdown_checked_now(")
        .unwrap();
    let timing_end = SHUTDOWN_TIGHTENING_MAIN
        .find("static int fixture_shutdown_observer(")
        .unwrap();
    source.push_str(&SHUTDOWN_TIGHTENING_MAIN[timing_start..timing_end]);
    let lifecycle_start = SHUTDOWN_TIGHTENING_MAIN
        .find("static void fixture_shutdown_join_observer(")
        .unwrap();
    let lifecycle_end = SHUTDOWN_TIGHTENING_MAIN.find("int main(void) {").unwrap();
    source.push_str(&SHUTDOWN_TIGHTENING_MAIN[lifecycle_start..lifecycle_end]);
    source.push_str(
        "static uint64_t fixture_numeric_generated_root(uint64_t now,uint64_t caller) {\n",
    );
    source.push_str(root_lines[0]);
    source.push_str("\n  return ku_task_driver_min(root,caller);\n}\n");
    source.push_str(SHUTDOWN_NUMERIC_MAIN);
    let directory = TempDir::new("task-driver-shutdown-numeric-range");
    let c_file = directory.path().join("shutdown-range.c");
    fs::write(&c_file, source).expect("write numeric deadline boundary witness");
    let Some(executable) = compile_harness(directory.path(), &c_file, "shutdown-range") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute the numeric shutdown boundary"
        );
        return;
    };
    fs::remove_file(c_file).expect("remove C before numeric witness execution");
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("numeric-boundary witness must release old untimed wait and reap");
    let stdout = String::from_utf8_lossy(&output.stdout).replace('\r', "");
    assert!(
        output.status.success(),
        "numeric-boundary verdict after real cleanup:\n{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.starts_with("task-driver-shutdown-range-cleanup-ok "));
    assert!(stdout.ends_with("task-driver-shutdown-range-ok\n"));
    assert_eq!(stdout.lines().count(), 2);
    assert!(output.stderr.is_empty());
}

const SHUTDOWN_NUMERIC_PROBE: &str = r#"
/* Deliberate numeric-boundary samples only on the shutdown controller.
 * Worker, harness, init, rollback and reaping clocks remain real. */
static KU_THREAD_LOCAL unsigned fixture_numeric_role,fixture_numeric_sample;
static KuTestEvent fixture_numeric_wait_entered;
static unsigned fixture_numeric_waits,fixture_numeric_os_waits,fixture_numeric_untimed;
static uint64_t fixture_numeric_passed,fixture_numeric_stored;
static uint64_t ku_task_driver_now_ms(void) {
  if (!fixture_numeric_role) return fixture_numeric_real_now();
  if (!fixture_numeric_sample) { fixture_numeric_sample=1; return UINT64_MAX-1000u; }
  return UINT64_MAX-1u;
}
static void fixture_numeric_os_path(uint64_t deadline) {
  if (!fixture_numeric_role) return;
  /* Called after the real deadline/clock prechecks, before the unchanged OS
   * branch. All these witness fields are under the actual driver mutex. */
  fixture_numeric_os_waits++;
  if (deadline==UINT64_MAX) fixture_numeric_untimed++;
}
static int ku_task_driver_wait(KuTaskDriverV1* driver,uint64_t deadline) {
  if (fixture_numeric_role) {
    fixture_numeric_waits++;
    if (fixture_numeric_waits==1u) {
      fixture_numeric_passed=deadline; fixture_numeric_stored=driver->shutdown_deadline;
      CHECK(ku_test_event_set(&fixture_numeric_wait_entered));
    }
  }
  return fixture_numeric_real_wait(driver,deadline); /* Never fabricate progress. */
}
static int fixture_numeric_shutdown(void* raw) {
  fixture_numeric_role=1; fixture_numeric_sample=0;
  uint32_t result=ku_task_driver_shutdown((KuTaskDriverV1*)raw,UINT64_MAX);
  fixture_numeric_role=0;
  return (int)result;
}
"#;

const SHUTDOWN_NUMERIC_MAIN: &str = r#"
int main(void) {
  CHECK(KU_TASK_DRIVER_ABI_VERSION==7u && KU_TASK_FRAME_ABI_VERSION==4u
      && KU_TASK_CONTROL_ABI_VERSION==2u);
  CHECK(ku_test_event_init(&fixture_numeric_wait_entered));
  const uint64_t startup_deadline=fixture_shutdown_checked_now()+1000u;
  FixtureDriver runtime; fixture_shutdown_runtime_init(&runtime,startup_deadline);
  KuTaskDriverTicketV1 building={0};
  CHECK(ku_task_driver_reserve(runtime.driver,fixture_task_bytes(),&building)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 before={0};
  CHECK(ku_task_driver_snapshot(runtime.driver,&before)==KU_TASK_DRIVER_OK);
  CHECK(before.worker_target==1u && before.workers_created==1u && !before.workers_exited
      && !before.workers_joined && before.resident==1u && before.building==1u
      && before.reserved_bytes==fixture_task_bytes() && !before.running && !before.queued
      && !before.fault && !before.clock_fault && !before.closing);
  KuTestThread controller;
  CHECK(ku_test_thread_start(&controller,fixture_numeric_shutdown,runtime.driver));
  int observed=ku_test_event_wait(&fixture_numeric_wait_entered,
      fixture_shutdown_remaining(startup_deadline));
  /* Lock acquisition proves the real waiter released the queue mutex (or its
   * fixed finite deadline already returned). Always recover BEFORE verdict. */
  CHECK(ku_task_driver_lock(runtime.driver)==0);
  const uint64_t passed=fixture_numeric_passed,stored=fixture_numeric_stored;
  const unsigned entries=fixture_numeric_waits,os_entries=fixture_numeric_os_waits;
  const unsigned untimed=fixture_numeric_untimed;
  CHECK(runtime.driver->closing && !runtime.driver->fault && !runtime.driver->clock_fault);
  CHECK(ku_task_driver_unlock(runtime.driver)==0);
  CHECK(ku_task_driver_rollback(&building)==KU_TASK_DRIVER_OK && !building.driver);
  fixture_shutdown_join_observer(&controller,startup_deadline);
  const int outcome=controller.outcome;
  CHECK(ku_task_driver_wait_idle(runtime.driver,startup_deadline)==KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 after={0};
  CHECK(ku_task_driver_snapshot(runtime.driver,&after)==KU_TASK_DRIVER_OK);
  CHECK(after.worker_target==1u && after.workers_created==1u && after.workers_exited==1u
      && !after.workers_joined && !after.workers_waiting && !after.resident && !after.building
      && !after.queued && !after.running && !after.parked && !after.retiring
      && !after.terminal_held && !after.reserved_bytes && !after.fault && !after.clock_fault);
  /* Already drained: this later, larger caller bound must not renew the first D. */
  CHECK(ku_task_driver_shutdown(runtime.driver,UINT64_MAX)==KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_lock(runtime.driver)==0);
  const uint64_t final_stored=runtime.driver->shutdown_deadline;
  CHECK(ku_task_driver_unlock(runtime.driver)==0);
  CHECK(ku_task_driver_destroy(runtime.driver)==KU_TASK_DRIVER_PENDING);
  fixture_shutdown_runtime_finish(&runtime,startup_deadline);
  CHECK(ku_test_event_destroy(&fixture_numeric_wait_entered));
  FixtureLedger ledger=fixture_ledger();
  CHECK(!ledger.allocations && !ledger.bytes && !ledger.overflow);
  CHECK(fixture_shutdown_checked_now()<=startup_deadline);
  printf("task-driver-shutdown-range-cleanup-ok passed=%llu stored=%llu outcome=%d os=%u untimed=%u ledger=0\n",
      (unsigned long long)passed,(unsigned long long)stored,outcome,os_entries,untimed);
  /* Exercise the ACTUAL generated root expression, not a copied implementation.
   * This scalar matrix complements, never replaces, the real group witness. */
  const uint64_t cases[][3]={
    {100u,UINT64_MAX,1100u},
    {UINT64_MAX-1002u,UINT64_MAX,UINT64_MAX-2u},
    {UINT64_MAX-1001u,UINT64_MAX,UINT64_MAX-1u},
    {UINT64_MAX-1000u,UINT64_MAX,UINT64_MAX-1u},
    {UINT64_MAX-999u,UINT64_MAX,UINT64_MAX-1u},
    {UINT64_MAX-1u,UINT64_MAX,UINT64_MAX-1u},
    {UINT64_MAX-1000u,UINT64_MAX-77u,UINT64_MAX-77u},
    {UINT64_MAX-1000u,0u,0u}
  };
  unsigned wrong=0;
  for (size_t i=0;i<sizeof(cases)/sizeof(cases[0]);i++)
    if (fixture_numeric_generated_root(cases[i][0],cases[i][1])!=cases[i][2]) wrong++;
  if (!observed || !entries || passed!=UINT64_MAX-1u || stored!=passed
      || final_stored!=stored || outcome!=KU_TASK_DRIVER_SHUTDOWN_TIMEOUT
      || os_entries || untimed || wrong) {
    fprintf(stderr,"task-driver-shutdown-range-after-cleanup: observed=%d entries=%u wrong=%u\n",
        observed,entries,wrong);
    return 1;
  }
  puts("task-driver-shutdown-range-ok"); return 0;
}
"#;
