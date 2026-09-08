//! Real worker join/close retry witnesses; not a Task execution or throughput test.
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
use native_harness::{compile_harness, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};
use std::{fs, process::Command};

fn fixture_source() -> String {
    let ast = Parser::new(Lexer::new("fn main() {}").lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let sync = ir::lower_program(&ast).unwrap();
    // One minimal verified frame requests the real runtime emitter. It is never
    // started: this gate isolates worker lifecycle resources from Task cleanup.
    let result = IrType::Result(Box::new(IrType::Null));
    let frames = TaskProgram {
        functions: vec![TaskFunction {
            id: TaskFunctionId(0),
            name: "JoinRetryUnusedFrame".into(),
            slots: vec![TaskSlot {
                ty: TaskSlotType::Value {
                    ty: result.clone(),
                    borrowed: false,
                },
            }],
            parameters: vec![],
            entry: StateId(0),
            result,
            states: vec![TaskState {
                operations: vec![TaskOp::Init {
                    dst: SlotId(0),
                    value: TaskConstant::Ok(Box::new(TaskConstant::Null)),
                }],
                terminator: TaskTerminator::Complete { value: SlotId(0) },
            }],
        }],
    };
    c::generate_task_frame_c_source(&sync, &frames).unwrap()
}

fn replace_once(source: String, anchor: &str, replacement: &str) -> String {
    assert_eq!(
        source.matches(anchor).count(),
        1,
        "join retry hook: {anchor}"
    );
    source.replacen(anchor, replacement, 1)
}

#[test]
fn native_task_driver_partial_join_close_failure_retries_original_deadline() {
    let generated = fixture_source();
    for forbidden in ["run_source", "const SOURCE", "task.spawn", "Task.new"] {
        assert!(!generated.contains(forbidden));
    }
    for required in [
        "ku_task_driver_join(",
        "KU_TASK_DRIVER_WORKER_EXITED",
        "workers_joined",
    ] {
        assert!(
            generated.contains(required),
            "missing lifecycle ABI: {required}"
        );
    }
    let source = replace_once(
        generated,
        "typedef struct KuString {",
        &format!("{ALLOCATION_HOOK}\ntypedef struct KuString {{"),
    );
    // Same native-call interception boundary as frame_c's startup fixture:
    // platform declarations/types are complete; real wrappers precede macros.
    let source = replace_once(
        source,
        "static uint64_t ku_task_driver_now_ms(void) {",
        &format!("{JOIN_RETRY_HOOK}\nstatic uint64_t ku_task_driver_now_ms(void) {{"),
    );
    let mut source = replace_once(
        source,
        "int main(void) {",
        "static int ku_generated_main(void) {",
    );
    source.push_str(JOIN_RETRY_MAIN);
    let directory = TempDir::new("task-driver-join-retry");
    let c_file = directory.path().join("join-retry.c");
    fs::write(&c_file, source).expect("write generated lifecycle retry fixture");
    let Some(executable) = compile_harness(directory.path(), &c_file, "join-retry") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute the native join/close retry fixture"
        );
        return;
    };
    fs::remove_file(&c_file).expect("remove source before native execution");
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("native lifecycle retry must remain bounded");
    assert!(
        output.status.success(),
        "native lifecycle retry failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-driver-join-retry-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const JOIN_RETRY_HOOK: &str = r#"
#define RETRY_CHECK(c) do { if (!(c)) { fprintf(stderr, "join retry line %d: %s\n", __LINE__, #c); abort(); } } while (0)
/* Only the exclusive init/join/destroy caller writes these records. Workers
 * execute the real empty driver and never call a fixture callback or allocator.
 * Successful native calls are never synthesized. Exactly one last-worker
 * operation fails BEFORE the OS call, retaining its actual resource. */
static uint64_t ku_task_driver_now_ms(void);
static uint64_t fixture_original_deadline;
static unsigned fixture_created, fixture_joined, fixture_closed, fixture_injected;
static unsigned fixture_stage, fixture_all_exited;
static void fixture_deadline_check(void) {
  uint64_t now = ku_task_driver_now_ms();
  RETRY_CHECK(now != UINT64_MAX && now <= fixture_original_deadline);
}
#if defined(_WIN32)
typedef struct FixtureRetryThread {
  HANDLE handle;
  void* context;
  unsigned wait_calls, joins, close_calls, closes;
} FixtureRetryThread;
static FixtureRetryThread fixture_threads[3];
static uintptr_t fixture_beginthreadex(void* security, unsigned stack_size,
    unsigned (__stdcall *entry)(void*), void* context, unsigned flags, unsigned* id) {
  RETRY_CHECK(!fixture_stage && fixture_created < 3u);
  uintptr_t created = _beginthreadex(security, stack_size, entry, context, flags, id);
  if (created) {
    FixtureRetryThread* record = &fixture_threads[fixture_created++];
    record->handle = (HANDLE)created; record->context = context;
  }
  return created;
}
static FixtureRetryThread* fixture_thread_for_handle(HANDLE handle) {
  for (unsigned index = 0; index < fixture_created; ++index)
    if (!fixture_threads[index].closes && fixture_threads[index].handle == handle)
      return &fixture_threads[index];
  RETRY_CHECK(false); return NULL;
}
static DWORD fixture_wait_thread(HANDLE handle, DWORD timeout) {
  RETRY_CHECK(fixture_stage == 1u && fixture_all_exited);
  FixtureRetryThread* record = fixture_thread_for_handle(handle);
  RETRY_CHECK(!record->joins && timeout != INFINITE && timeout <= 1000u);
  fixture_deadline_check(); record->wait_calls++;
  DWORD result = WaitForSingleObject(handle, timeout);
  if (result == WAIT_OBJECT_0) { record->joins++; fixture_joined++; }
  return result;
}
static BOOL fixture_close_thread(HANDLE handle) {
  RETRY_CHECK((fixture_stage == 1u || fixture_stage == 2u) && fixture_all_exited);
  FixtureRetryThread* record = fixture_thread_for_handle(handle);
  RETRY_CHECK(record->joins == 1u && !record->closes);
  RETRY_CHECK(fixture_stage != 2u || record == &fixture_threads[2]);
  fixture_deadline_check(); record->close_calls++;
  if (record == &fixture_threads[2] && !fixture_injected) {
    RETRY_CHECK(fixture_stage == 1u && record->close_calls == 1u);
    RETRY_CHECK(fixture_joined == 3u && fixture_closed == 2u);
    fixture_injected++;
    SetLastError(ERROR_ACCESS_DENIED);
    return FALSE; /* Injected once; do NOT call CloseHandle or free anything. */
  }
  BOOL closed = CloseHandle(handle);
  if (closed) { record->closes++; fixture_closed++; }
  return closed;
}
#define _beginthreadex fixture_beginthreadex
#define WaitForSingleObject fixture_wait_thread
#define CloseHandle fixture_close_thread
#else
typedef struct FixtureRetryThread {
  pthread_t handle;
  void* context;
  unsigned join_calls, joins;
} FixtureRetryThread;
static FixtureRetryThread fixture_threads[3];
static int fixture_pthread_create(pthread_t* thread, const pthread_attr_t* attr,
                                  void* (*entry)(void*), void* context) {
  RETRY_CHECK(!fixture_stage && fixture_created < 3u);
  int result = pthread_create(thread, attr, entry, context);
  if (!result) {
    FixtureRetryThread* record = &fixture_threads[fixture_created++];
    record->handle = *thread; record->context = context;
  }
  return result;
}
static int fixture_pthread_join(pthread_t handle, void** output) {
  RETRY_CHECK((fixture_stage == 1u || fixture_stage == 2u) && fixture_all_exited);
  FixtureRetryThread* record = NULL;
  for (unsigned index = 0; index < fixture_created; ++index)
    if (!fixture_threads[index].joins && pthread_equal(fixture_threads[index].handle, handle)) {
      record = &fixture_threads[index]; break;
    }
  RETRY_CHECK(record != NULL && !record->joins);
  RETRY_CHECK(fixture_stage != 2u || record == &fixture_threads[2]);
  fixture_deadline_check(); record->join_calls++;
  if (record == &fixture_threads[2] && !fixture_injected) {
    RETRY_CHECK(fixture_stage == 1u && record->join_calls == 1u);
    RETRY_CHECK(fixture_joined == 2u && fixture_closed == 2u);
    fixture_injected++;
    return EINVAL; /* Injected once before pthread_join; the thread is retained. */
  }
  int joined = pthread_join(handle, output);
  if (!joined) { record->joins++; fixture_joined++; fixture_closed++; }
  return joined;
}
#define pthread_create fixture_pthread_create
#define pthread_join fixture_pthread_join
#endif
"#;

const JOIN_RETRY_MAIN: &str = r#"
static void fixture_fixed_ledger(size_t fixed) {
  RETRY_CHECK(ku_perf_calls == 3u && ku_perf_live_allocations == 3u);
  RETRY_CHECK(ku_perf_live_bytes == fixed && ku_perf_peak_bytes == fixed);
  RETRY_CHECK(ku_perf_total_bytes == fixed && !ku_perf_overflow);
}
static KuTaskDriverSnapshotV1 fixture_snapshot(KuTaskDriverV1* driver, size_t fixed) {
  KuTaskDriverSnapshotV1 snapshot = {0};
  RETRY_CHECK(ku_task_driver_snapshot(driver, &snapshot) == KU_TASK_DRIVER_OK);
  RETRY_CHECK(snapshot.worker_target == 3u && snapshot.workers_created == 3u);
  RETRY_CHECK(!snapshot.fault && !snapshot.clock_fault);
  RETRY_CHECK(!snapshot.resident && !snapshot.building && !snapshot.queued
      && !snapshot.running && !snapshot.reserved_bytes);
  RETRY_CHECK(!snapshot.polls && snapshot.fixed_bytes == fixed && snapshot.byte_limit == fixed);
  return snapshot;
}
static void fixture_live_storage(KuTaskDriverV1* driver, size_t fixed) {
  RETRY_CHECK(driver->initialized == KU_TASK_DRIVER_STORAGE_LIVE);
  RETRY_CHECK(driver->sync_resources == (KU_TASK_DRIVER_SYNC_MUTEX
      | KU_TASK_DRIVER_SYNC_STATE | KU_TASK_DRIVER_SYNC_WORK));
  RETRY_CHECK(driver->shutdown_deadline == fixture_original_deadline);
  fixture_fixed_ledger(fixed);
}
int main(void) {
  RETRY_CHECK(KU_TASK_DRIVER_ABI_VERSION == 7u);
  RETRY_CHECK(KU_TASK_FRAME_ABI_VERSION == 4u && KU_TASK_CONTROL_ABI_VERSION == 2u);
  KuTaskDriverV1* driver = (KuTaskDriverV1*)calloc(1, sizeof(*driver));
  KuTaskDriverSlotV1* slots = (KuTaskDriverSlotV1*)calloc(2u, sizeof(*slots));
  size_t* ring = (size_t*)calloc(2u, sizeof(*ring));
  RETRY_CHECK(driver && slots && ring);
  size_t fixed = sizeof(*driver) + 2u * (sizeof(*slots) + sizeof(*ring));
  fixture_fixed_ledger(fixed);
  uint64_t now = ku_task_driver_now_ms();
  RETRY_CHECK(now != UINT64_MAX && now < UINT64_MAX - 1000u);
  const uint64_t deadline = now + 1000u;
  fixture_original_deadline = deadline;
  RETRY_CHECK(ku_task_driver_init(driver, sizeof(*driver), KU_TASK_DRIVER_ABI_VERSION,
      slots, 2u, ring, 2u, fixed, 3u, deadline) == KU_TASK_DRIVER_OK);
  RETRY_CHECK(fixture_created == 3u && !fixture_joined && !fixture_closed && !fixture_injected);
  RETRY_CHECK(ku_task_driver_wait_idle(driver, deadline) == KU_TASK_DRIVER_OK);
  KuTaskDriverSnapshotV1 snapshot = fixture_snapshot(driver, fixed);
  RETRY_CHECK(!snapshot.closing && snapshot.workers_waiting == 3u
      && !snapshot.workers_exited && !snapshot.workers_joined);
  RETRY_CHECK(ku_task_driver_shutdown(driver, deadline) == KU_TASK_DRIVER_OK);
  snapshot = fixture_snapshot(driver, fixed);
  RETRY_CHECK(snapshot.closing && snapshot.workers_exited == 3u
      && !snapshot.workers_waiting && !snapshot.workers_joined);
  /* Actual runtime observations, never writes to worker state or fake ACK.
   * All normal worker storage access has finished, so teardown is exclusive. */
  for (unsigned index = 0; index < 3u; ++index) {
    RETRY_CHECK(driver->workers[index].driver == driver && driver->workers[index].index == index);
    RETRY_CHECK(driver->workers[index].state == KU_TASK_DRIVER_WORKER_EXITED);
    RETRY_CHECK(!driver->workers[index].joined && !driver->workers[index].closed);
    RETRY_CHECK(fixture_threads[index].context == &driver->workers[index]);
  }
  fixture_all_exited = 1u; fixture_live_storage(driver, fixed);
  RETRY_CHECK(ku_task_driver_destroy(driver) == KU_TASK_DRIVER_PENDING);
  fixture_live_storage(driver, fixed);
  fixture_stage = 1u;
  fixture_deadline_check();
  RETRY_CHECK(ku_task_driver_join(driver, deadline) == KU_TASK_DRIVER_INTERNAL);
  RETRY_CHECK(fixture_injected == 1u && fixture_closed == 2u);
  fixture_live_storage(driver, fixed);
  snapshot = fixture_snapshot(driver, fixed);
  RETRY_CHECK(snapshot.workers_exited == 3u && !snapshot.workers_waiting);
  for (unsigned index = 0; index < 2u; ++index) {
    RETRY_CHECK(driver->workers[index].joined && driver->workers[index].closed);
    RETRY_CHECK(fixture_threads[index].joins == 1u);
  }
#if defined(_WIN32)
  RETRY_CHECK(fixture_joined == 3u && snapshot.workers_joined == 3u);
  RETRY_CHECK(driver->workers[2].joined && !driver->workers[2].closed);
  RETRY_CHECK(driver->workers[2].thread == fixture_threads[2].handle);
  RETRY_CHECK(!driver->workers[0].thread && !driver->workers[1].thread);
  DWORD flags = 0;
  RETRY_CHECK(GetHandleInformation(driver->workers[2].thread, &flags));
  unsigned before_waits[3];
  for (unsigned index = 0; index < 3u; ++index) {
    RETRY_CHECK(fixture_threads[index].joins == 1u && fixture_threads[index].close_calls == 1u);
    RETRY_CHECK(fixture_threads[index].closes == (index < 2u ? 1u : 0u));
    RETRY_CHECK(fixture_threads[index].wait_calls >= 1u && fixture_threads[index].wait_calls <= 2u);
    before_waits[index] = fixture_threads[index].wait_calls;
  }
#else
  RETRY_CHECK(fixture_joined == 2u && snapshot.workers_joined == 2u);
  RETRY_CHECK(!driver->workers[2].joined && !driver->workers[2].closed);
  for (unsigned index = 0; index < 3u; ++index) {
    RETRY_CHECK(fixture_threads[index].join_calls == 1u);
    RETRY_CHECK(fixture_threads[index].joins == (index < 2u ? 1u : 0u));
  }
#endif
  /* Even Windows' all-three-JOINED state is not sufficient: the final actual
   * handle is still owned. Pending must not destroy synchronization/storage. */
  RETRY_CHECK(ku_task_driver_destroy(driver) == KU_TASK_DRIVER_PENDING);
  fixture_live_storage(driver, fixed);
  RETRY_CHECK(fixture_injected == 1u && fixture_closed == 2u);
  fixture_stage = 2u; fixture_deadline_check();
  RETRY_CHECK(ku_task_driver_join(driver, deadline) == KU_TASK_DRIVER_OK);
  RETRY_CHECK(fixture_joined == 3u && fixture_closed == 3u && fixture_injected == 1u);
  fixture_live_storage(driver, fixed);
  snapshot = fixture_snapshot(driver, fixed);
  RETRY_CHECK(snapshot.workers_exited == 3u && snapshot.workers_joined == 3u
      && !snapshot.workers_waiting);
  for (unsigned index = 0; index < 3u; ++index) {
    RETRY_CHECK(driver->workers[index].joined && driver->workers[index].closed);
    RETRY_CHECK(fixture_threads[index].joins == 1u);
#if defined(_WIN32)
    RETRY_CHECK(!driver->workers[index].thread && fixture_threads[index].closes == 1u);
    RETRY_CHECK(fixture_threads[index].wait_calls == before_waits[index]);
    RETRY_CHECK(fixture_threads[index].close_calls == (index == 2u ? 2u : 1u));
#else
    RETRY_CHECK(fixture_threads[index].join_calls == (index == 2u ? 2u : 1u));
#endif
  }
  /* A further idempotent call cannot invoke ANY native wait/join/close wrapper. */
  fixture_stage = 3u;
  RETRY_CHECK(ku_task_driver_join(driver, deadline) == KU_TASK_DRIVER_OK);
  RETRY_CHECK(fixture_joined == 3u && fixture_closed == 3u && fixture_injected == 1u);
  fixture_live_storage(driver, fixed);
  RETRY_CHECK(ku_task_driver_destroy(driver) == KU_TASK_DRIVER_OK);
  RETRY_CHECK(driver->initialized == KU_TASK_DRIVER_STORAGE_ZERO && !driver->sync_resources);
  fixture_fixed_ledger(fixed);
  RETRY_CHECK(ku_task_frame_zero_bytes(slots, 2u * sizeof(*slots)));
  RETRY_CHECK(ku_task_frame_zero_bytes(ring, 2u * sizeof(*ring)));
  free(ring); free(slots); free(driver);
  RETRY_CHECK(ku_perf_calls == 3u && !ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  /* This actual test must finish within its original finite D. It is not a
   * hard real-time guarantee for portable POSIX pthread_join's OS-return tail. */
  fixture_deadline_check();
  puts("task-driver-join-retry-ok"); return 0;
}
"#;
