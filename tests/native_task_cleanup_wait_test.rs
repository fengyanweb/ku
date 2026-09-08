//! R5b.2 internal ACK waits over real R4/R2/R3 tasks; not source scope lowering.
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
            name: "CleanupWaitOwnedFrame".into(),
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
    assert_eq!(
        source.matches(anchor).count(),
        1,
        "cleanup wait hook: {anchor}"
    );
    source.replacen(anchor, replacement, 1)
}

#[test]
fn native_task_cleanup_wait_deadlines_cancel_and_publication_execute_in_c() {
    let generated = fixture_source();
    for forbidden in ["run_source", "const SOURCE", "task.spawn", "Task.new"] {
        assert!(!generated.contains(forbidden));
    }
    for required in [
        "ku_task_driver_cleanup_wait_arm(",
        "KU_TASK_DRIVER_WAIT_KIND_CLEANUP_ACK",
        "KU_TASK_DRIVER_CLEANUP_TIMEOUT",
        "scope_wait_failure",
    ] {
        assert!(
            generated.contains(required),
            "missing ACK wait ABI: {required}"
        );
    }
    let instrumentation = format!("{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{LEDGER_LOCK}\n{ALLOCATION_HOOK}\n{LOCKED_ALLOCATIONS}\n{HOOK_DECLARATIONS}\n");
    let source = replace_once(
        generated,
        "typedef struct KuString {",
        &format!("{instrumentation}typedef struct KuString {{"),
    );
    let source = replace_once(
        source,
        "static uint64_t ku_task_driver_now_ms(void) {",
        &format!("{CLOCK_HOOK}\nstatic uint64_t fixture_original_now(void) {{"),
    );
    let source = replace_once(source, "static uint32_t ku_task_0_resume(void* raw) {", "static uint32_t ku_task_0_resume(void* raw) {\n  uint32_t observed = fixture_resume(raw);\n  if (observed != UINT32_MAX) return observed;");
    let source = replace_once(source, "static uint32_t ku_task_0_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {", "static uint32_t ku_task_0_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {\n  uint32_t observed = fixture_cleanup(raw, reason);\n  if (observed != UINT32_MAX) return observed;");
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
    let source = replace_once(source, "static uint32_t ku_task_0_take_payload(void* raw, void* destination) {", "static uint32_t ku_task_0_take_payload(void* raw, void* destination) {\n  uint32_t observed = fixture_take(raw);\n  if (observed != UINT32_MAX) return observed;");
    let source = replace_once(source, "      ? KU_TASK_CONTROL_PAYLOAD_TAKEN : KU_TASK_CONTROL_PAYLOAD_AVAILABLE);\n  return taken;", "      ? KU_TASK_CONTROL_PAYLOAD_TAKEN : KU_TASK_CONTROL_PAYLOAD_AVAILABLE);\n  fixture_after_store(control);\n  return taken;");
    let source = replace_once(source, "      ku_task_control_deadline_store(&control->cleanup_deadline_ms, absolute_deadline_ms);", "      fixture_publishing(control);\n      ku_task_control_deadline_store(&control->cleanup_deadline_ms, absolute_deadline_ms);");
    let source = replace_once(source, "  if (slot->cleanup_fault) return slot->cleanup_fault;\n  KuTaskControlV1* control = slot->driver_lease.control;", "  if (slot->cleanup_fault) return slot->cleanup_fault;\n  fixture_before_ack(slot->driver_lease.control);\n  KuTaskControlV1* control = slot->driver_lease.control;");
    let source = replace_once(source, "  slot->cleanup_acked_generation = slot->generation;\n  return KU_TASK_DRIVER_CLEANUP_ACK;", "  slot->cleanup_acked_generation = slot->generation;\n  fixture_ack(slot->driver_lease.control);\n  return KU_TASK_DRIVER_CLEANUP_ACK;");
    let source = replace_once(source, "static void ku_task_0_dispose(KuTaskControlV1* control, void* raw) {", "static void ku_task_0_dispose(KuTaskControlV1* control, void* raw) {\n  void* witness = fixture_before_dispose(raw);");
    let source = replace_once(source, "    ku_task_adapter_fault(ticket.driver, 0);\n}\nstatic const KuTaskControlOpsV1 ku_task_0_ops", "    ku_task_adapter_fault(ticket.driver, 0);\n  fixture_disposed(witness);\n}\nstatic const KuTaskControlOpsV1 ku_task_0_ops");
    let mut source = replace_once(
        source,
        "int main(void) {",
        "static int ku_generated_main(void) {",
    );
    source.push_str(C_MAIN);
    let directory = TempDir::new("task-cleanup-wait-v1");
    let c_file = directory.path().join("cleanup_wait.c");
    fs::write(&c_file, source).unwrap();
    let Some(executable) = compile_harness(directory.path(), &c_file, "cleanup_wait") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute native ACK waits"
        );
        return;
    };
    fs::remove_file(c_file).unwrap();
    for (argument, expected) in [
        (None, "task-cleanup-wait-v1-ok\n"),
        (
            Some("--corrupt-ack"),
            "task-cleanup-wait-quarantined-not-drained\n",
        ),
    ] {
        let mut command = Command::new(&executable);
        command.current_dir(directory.path());
        if let Some(argument) = argument {
            command.arg(argument);
        }
        let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS)
            .expect("ACK waits must remain bounded");
        assert!(
            output.status.success(),
            "ACK wait fixture {argument:?}:\n{}{}",
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
#define CHECK(c) do { if (!(c)) { fprintf(stderr, "cleanup wait check line %d: %s\n", __LINE__, #c); abort(); } } while (0)
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
static void* fixture_malloc(size_t size) { fixture_alloc_lock(); void* p = ku_perf_malloc(size); fixture_alloc_unlock(); return p; }
static void* fixture_calloc(size_t count, size_t size) { fixture_alloc_lock(); void* p = ku_perf_calloc(count, size); fixture_alloc_unlock(); return p; }
static void* fixture_realloc(void* old, size_t size) { fixture_alloc_lock(); void* p = ku_perf_realloc(old, size); fixture_alloc_unlock(); return p; }
static void fixture_free(void* p) { fixture_alloc_lock(); ku_perf_free(p); fixture_alloc_unlock(); }
typedef struct FixtureLedger { size_t allocations, bytes, calls; int overflow; } FixtureLedger;
static FixtureLedger fixture_ledger(void) {
  fixture_alloc_lock(); FixtureLedger out = {ku_perf_live_allocations, ku_perf_live_bytes, ku_perf_calls, ku_perf_overflow};
  fixture_alloc_unlock(); return out;
}
#define malloc fixture_malloc
#define calloc fixture_calloc
#define realloc fixture_realloc
#define free fixture_free
"#;

const HOOK_DECLARATIONS: &str = r#"
static uint32_t fixture_resume(void*);
static uint32_t fixture_cleanup(void*, uint32_t);
static void fixture_drop_frame(void*);
static void fixture_drop_payload(void*);
static uint32_t fixture_take(void*);
static void fixture_after_store(void*);
static void fixture_publishing(void*);
static void fixture_before_ack(void*);
static void fixture_ack(void*);
static void* fixture_before_dispose(void*);
static void fixture_disposed(void*);
"#;

const CLOCK_HOOK: &str = r#"
static KuTaskControlDeadlineV1 fixture_clock;
static uint64_t fixture_original_now(void);
static uint64_t ku_task_driver_now_ms(void) {
  uint64_t value = ku_task_control_deadline_load(&fixture_clock);
  return value ? value : fixture_original_now();
}
"#;

const C_MAIN: &str = r#"
#define ROLE_COUNT 12u
enum { MODE_FINISH, MODE_HOLD, MODE_WAIT, MODE_PROBE };
enum { PROBE_HEADERS, PROBE_SELF_ACK, PROBE_CYCLE_ACK, PROBE_FAILURE, PROBE_FIRST_ACK };
typedef struct FixtureDriver { KuTaskDriverV1* driver; KuTaskDriverSlotV1* slots; size_t* ring; size_t capacity; } FixtureDriver;
typedef struct FixtureRole {
  KuTaskHandle_0 handle;
  KuTaskDriverTicketV1 ticket;
  KuTaskDriverCleanupReceiptV1 receipt, target_receipt;
  KuTaskDriverWaitTokenV1 wait, old_wait;
  KuTestEvent entered, proceed, armed, finished, cleanup_entered, acked, disposed;
  KuTestEvent take_entered, take_proceed, stored, store_proceed;
  KuTestEvent publishing, publish_proceed;
  KuAtomicRefcount resumes, continuations, cleanups, frame_drops, payload_drops, acks, disposes;
  uint64_t deadline;
  uint32_t mode, probe, outcome, arm_status, reason;
  size_t target;
  int initialized, done, entry_gate, after_arm_gate, hold_latch, cleanup_hold, wait_cleanup;
  int take_gate, take_reject, store_gate, corrupt_ack, queue_full, publishing_gate;
} FixtureRole;
static FixtureRole roles[ROLE_COUNT];
/* The fault subprocess explicitly retains this root. Reachability is not drain. */
static FixtureDriver fixture_quarantined_driver;
static size_t fixture_count(KuAtomicRefcount* p) { return ku_task_control_atomic_load(p); }
static void fixture_inc(KuAtomicRefcount* p) { ku_task_control_atomic_store(p, fixture_count(p) + 1u); }
static uint64_t fixture_deadline(void) {
  uint64_t now = ku_task_driver_now_ms(); return now == UINT64_MAX ? 0 : now + 2000u;
}
static FixtureRole* fixture_role(void* raw) {
  KuTaskInstance_0* instance = (KuTaskInstance_0*)raw;
  CHECK(instance->frame.s_0 >= 0 && instance->frame.s_0 < ROLE_COUNT);
  FixtureRole* role = &roles[(size_t)instance->frame.s_0]; CHECK(role->initialized); return role;
}
static void fixture_role_init(size_t i, uint32_t mode) {
  CHECK(i < ROLE_COUNT && !roles[i].initialized); FixtureRole* r = &roles[i]; memset(r, 0, sizeof(*r));
  CHECK(ku_test_event_init(&r->entered)); CHECK(ku_test_event_init(&r->proceed));
  CHECK(ku_test_event_init(&r->armed)); CHECK(ku_test_event_init(&r->finished)); CHECK(ku_test_event_init(&r->cleanup_entered));
  CHECK(ku_test_event_init(&r->acked)); CHECK(ku_test_event_init(&r->disposed));
  CHECK(ku_test_event_init(&r->take_entered)); CHECK(ku_test_event_init(&r->take_proceed));
  CHECK(ku_test_event_init(&r->stored)); CHECK(ku_test_event_init(&r->store_proceed));
  CHECK(ku_test_event_init(&r->publishing)); CHECK(ku_test_event_init(&r->publish_proceed));
  ku_task_control_atomic_init(&r->resumes, 0); ku_task_control_atomic_init(&r->continuations, 0);
  ku_task_control_atomic_init(&r->cleanups, 0); ku_task_control_atomic_init(&r->frame_drops, 0);
  ku_task_control_atomic_init(&r->payload_drops, 0); ku_task_control_atomic_init(&r->acks, 0); ku_task_control_atomic_init(&r->disposes, 0);
  r->mode = mode; r->initialized = 1;
}
static KuTaskDriverSnapshotV1 fixture_snapshot(FixtureDriver* runtime, uint32_t fault) {
  KuTaskDriverSnapshotV1 s = {0}; CHECK(ku_task_driver_snapshot(runtime->driver, &s) == KU_TASK_DRIVER_OK);
  CHECK(s.fault == fault && s.running <= 1 && s.resident <= runtime->capacity); return s;
}
static void fixture_idle(FixtureDriver* runtime, uint32_t fault) {
  CHECK(ku_task_driver_wait_idle(runtime->driver, fixture_deadline()) == fault);
  KuTaskDriverSnapshotV1 s = fixture_snapshot(runtime, fault);
  CHECK(!s.queued && !s.running && (s.worker_waiting || s.worker_exited));
}
static void fixture_driver_init(FixtureDriver* runtime, size_t capacity) {
  memset(runtime, 0, sizeof(*runtime)); runtime->capacity = capacity;
  runtime->driver = (KuTaskDriverV1*)calloc(1, sizeof(*runtime->driver));
  runtime->slots = (KuTaskDriverSlotV1*)calloc(capacity, sizeof(*runtime->slots));
  runtime->ring = (size_t*)calloc(capacity, sizeof(*runtime->ring)); CHECK(runtime->driver && runtime->slots && runtime->ring);
  size_t fixed = sizeof(*runtime->driver) + capacity * (sizeof(*runtime->slots) + sizeof(*runtime->ring));
  for (uint32_t old = 1; old < 5; ++old) {
    CHECK(ku_task_driver_init(runtime->driver, sizeof(*runtime->driver), old, runtime->slots, capacity, runtime->ring, capacity, fixed + 1048576u) == KU_TASK_DRIVER_ABI_MISMATCH);
    CHECK(ku_task_frame_zero_bytes(runtime->driver, sizeof(*runtime->driver)));
    CHECK(ku_task_frame_zero_bytes(runtime->slots, capacity * sizeof(*runtime->slots)));
    CHECK(ku_task_frame_zero_bytes(runtime->ring, capacity * sizeof(*runtime->ring)));
  }
  CHECK(ku_task_driver_init(runtime->driver, sizeof(*runtime->driver), 5u, runtime->slots, capacity, runtime->ring, capacity, fixed + 1048576u) == KU_TASK_DRIVER_OK);
  fixture_idle(runtime, 0); CHECK(fixture_snapshot(runtime, 0).fixed_bytes == fixed);
}
static void fixture_clock_begin(void) {
  /* Virtual time controls selected interleavings, not real timer performance.
   * Windows/macOS relative waits can expire while virtual time is frozen.
   * Events/joins have real 2s limits; driver waits using this frozen clock
   * instead rely on the real 20s process watchdog if their progress breaks.
   * A separate real-clock case checks automatic deadline wakeup. */
  ku_task_control_deadline_store(&fixture_clock, ku_test_real_now_ms() + 100000u);
}
static void fixture_advance(FixtureDriver* runtime, uint64_t now) {
  CHECK(!ku_task_driver_lock(runtime->driver));
  CHECK(now >= ku_task_control_deadline_load(&fixture_clock));
  ku_task_control_deadline_store(&fixture_clock, now);
  /* Invoke the actual locked timer scan for this synthetic-time transaction.
   * A signal + wait_idle alone could observe the old worker_waiting flag
   * before the worker has processed that signal. The real-clock case below
   * separately exercises the worker's automatic OS timer wakeup. */
  (void)ku_task_driver_deadlines_locked(runtime->driver, now);
  ku_task_driver_signal(runtime->driver);
  CHECK(!ku_task_driver_unlock(runtime->driver));
}
static void fixture_start(FixtureDriver* runtime, size_t i) {
  int64_t id = (int64_t)i; uint8_t* text = (uint8_t*)malloc(7); CHECK(text); memcpy(text, "payload", 7);
  KuResult_str input = {true, {text, 7, 7, KU_STRING_OWNED}, {0}};
  CHECK(ku_task_0_try_start(runtime->driver, &id, &input, &roles[i].handle) == KU_TASK_DRIVER_OK);
  CHECK(!input.value.ptr); roles[i].ticket = roles[i].handle.ticket;
}
static void fixture_transfer(size_t i, uint64_t deadline) {
  FixtureRole* r = &roles[i]; size_t calls = fixture_ledger().calls;
  CHECK(ku_task_driver_owner_drop_receipt(&r->ticket, &r->handle.owner, deadline, &r->receipt) == KU_TASK_DRIVER_OK);
  CHECK(!r->handle.owner.lease.control && r->receipt.generation == r->ticket.generation);
  r->handle.ticket = (KuTaskDriverTicketV1){0}; CHECK(fixture_ledger().calls == calls);
}
static void fixture_disposed_wait(size_t i) {
  CHECK(ku_test_event_wait(&roles[i].disposed, 2000)); CHECK(fixture_count(&roles[i].disposes) == 1);
  CHECK(ku_task_driver_cleanup_receipt_read(&roles[i].receipt) == KU_TASK_DRIVER_CLEANUP_ACK);
}
static void fixture_set_mode(FixtureDriver* runtime, size_t i, uint32_t mode) {
  CHECK(!ku_task_driver_lock(runtime->driver));
  CHECK(runtime->slots[roles[i].ticket.slot].state != KU_TASK_DRIVER_RUNNING);
  roles[i].mode = mode; CHECK(!ku_task_driver_unlock(runtime->driver));
  CHECK(ku_task_driver_wake(&roles[i].ticket) == KU_TASK_DRIVER_OK);
}
static void fixture_child_release(FixtureDriver* runtime, size_t i) {
  CHECK(!ku_task_driver_lock(runtime->driver));
  CHECK(runtime->slots[roles[i].ticket.slot].state != KU_TASK_DRIVER_RUNNING);
  roles[i].cleanup_hold = 0; CHECK(!ku_task_driver_unlock(runtime->driver));
  CHECK(ku_task_driver_wake(&roles[i].ticket) == KU_TASK_DRIVER_OK);
}
static KuTaskDriverWaitSnapshotV1 fixture_wait_snapshot(size_t i) {
  KuTaskDriverWaitSnapshotV1 s = {0}; CHECK(ku_task_driver_wait_read(&roles[i].wait, &s) == KU_TASK_DRIVER_OK);
  CHECK(s.kind == KU_TASK_DRIVER_WAIT_KIND_CLEANUP_ACK); return s;
}
static void fixture_armed(FixtureDriver* runtime, size_t i) {
  CHECK(ku_test_event_wait(&roles[i].armed, 2000)); fixture_idle(runtime, 0);
  KuTaskDriverWaitSnapshotV1 s = fixture_wait_snapshot(i);
  CHECK(s.state == KU_TASK_DRIVER_WAIT_ARMED && s.outcome == KU_TASK_DRIVER_PENDING);
}
static void fixture_finish(FixtureDriver* runtime, uint32_t fault) {
  for (size_t i = 0; i < ROLE_COUNT; ++i) if (roles[i].initialized) {
    CHECK(!roles[i].handle.owner.lease.control);
    CHECK(ku_test_event_wait(&roles[i].disposed, 2000) && fixture_count(&roles[i].disposes) == 1);
    /* A multi-driver preflight case has already destroyed its other driver;
     * its event/counters are still valid, its receipt lifetime is not. */
    if (roles[i].receipt.driver == runtime->driver)
      CHECK(ku_task_driver_cleanup_receipt_read(&roles[i].receipt) == KU_TASK_DRIVER_CLEANUP_ACK);
  }
  CHECK(ku_task_driver_shutdown(runtime->driver, fixture_deadline()) == fault);
  KuTaskDriverSnapshotV1 s = fixture_snapshot(runtime, fault);
  CHECK(!s.resident && !s.reserved_bytes && !s.queued && !s.running && !s.building);
  uint64_t end = ku_test_real_now_ms() + 2000u; uint32_t destroyed = KU_TASK_DRIVER_PENDING;
  for (size_t i = 0; i < 4096 && destroyed == KU_TASK_DRIVER_PENDING; ++i) {
    destroyed = ku_task_driver_destroy(runtime->driver);
    if (destroyed == KU_TASK_DRIVER_PENDING) { CHECK(ku_test_real_now_ms() < end); ku_test_thread_yield(); }
  }
  CHECK(destroyed == KU_TASK_DRIVER_OK); free(runtime->ring); free(runtime->slots); free(runtime->driver);
  for (size_t i = 0; i < ROLE_COUNT; ++i) if (roles[i].initialized) {
    FixtureRole* r = &roles[i]; CHECK(fixture_count(&r->frame_drops) == 1 && fixture_count(&r->disposes) == 1);
    CHECK(ku_test_event_destroy(&r->entered)); CHECK(ku_test_event_destroy(&r->proceed));
    CHECK(ku_test_event_destroy(&r->armed)); CHECK(ku_test_event_destroy(&r->finished)); CHECK(ku_test_event_destroy(&r->cleanup_entered));
    CHECK(ku_test_event_destroy(&r->acked)); CHECK(ku_test_event_destroy(&r->disposed));
    CHECK(ku_test_event_destroy(&r->take_entered)); CHECK(ku_test_event_destroy(&r->take_proceed));
    CHECK(ku_test_event_destroy(&r->stored)); CHECK(ku_test_event_destroy(&r->store_proceed));
    CHECK(ku_test_event_destroy(&r->publishing)); CHECK(ku_test_event_destroy(&r->publish_proceed)); memset(r, 0, sizeof(*r));
  }
  memset(runtime, 0, sizeof(*runtime)); ku_task_control_deadline_store(&fixture_clock, 0);
  FixtureLedger ledger = fixture_ledger(); CHECK(!ledger.allocations && !ledger.bytes && !ledger.overflow);
}
static void fixture_probe(FixtureRole*, KuTaskInstance_0*);
static uint32_t fixture_wait_step(FixtureRole* r, KuTaskInstance_0* instance, int cleanup) {
  if (r->done) return UINT32_MAX;
  uint32_t outcome;
  if (!r->wait.driver) {
    if (r->queue_full) {
      CHECK(!ku_task_driver_lock(instance->ticket.driver));
      CHECK(instance->ticket.driver->queued == instance->ticket.driver->capacity - 1u);
      CHECK(!ku_task_driver_unlock(instance->ticket.driver));
    }
    size_t calls = fixture_ledger().calls;
    outcome = ku_task_driver_cleanup_wait_arm(&instance->ticket, &r->target_receipt, r->deadline, &r->wait);
    CHECK(fixture_ledger().calls == calls);
    r->arm_status = outcome; CHECK(ku_test_event_set(&r->armed));
    if (r->after_arm_gate) { CHECK(ku_test_event_set(&r->entered)); CHECK(ku_test_event_wait(&r->proceed, 2000)); }
    if (outcome == KU_TASK_DRIVER_PENDING) return KU_TASK_CONTROL_PENDING;
  } else {
    KuTaskDriverWaitSnapshotV1 s = {0}; CHECK(ku_task_driver_wait_read(&r->wait, &s) == KU_TASK_DRIVER_OK);
    CHECK(s.kind == KU_TASK_DRIVER_WAIT_KIND_CLEANUP_ACK);
    if (s.state == KU_TASK_DRIVER_WAIT_ARMED || r->hold_latch) {
      CHECK(ku_task_driver_set_intent(&instance->ticket, KU_TASK_DRIVER_WAIT) == KU_TASK_DRIVER_OK); return KU_TASK_CONTROL_PENDING;
    }
    CHECK(s.state == KU_TASK_DRIVER_WAIT_NOTIFIED); outcome = s.outcome;
    r->old_wait = r->wait; CHECK(ku_task_driver_wait_detach(&r->wait) == KU_TASK_DRIVER_OK);
  }
  r->outcome = outcome; r->done = 1; fixture_inc(&r->continuations);
  if (!cleanup && outcome != KU_TASK_DRIVER_CLEANUP_ACK) {
    CHECK(outcome == KU_TASK_DRIVER_CLEANUP_TIMEOUT || outcome == KU_TASK_DRIVER_INTERNAL);
    /* Fixture-only caller error mapping; real owned slot drop/Result moves
     * remain R4 generated glue, not a claim that b.3 source lowering exists. */
    ku_result_drop_str(&instance->frame.s_1);
    instance->frame.s_1 = (KuResult_str){false, {0}, {
      ku_string_static((const uint8_t*)"task", 4),
      outcome == KU_TASK_DRIVER_CLEANUP_TIMEOUT ? ku_string_static((const uint8_t*)"shutdown_timeout", 16)
          : ku_string_static((const uint8_t*)"fixture_cleanup_fault", 21),
      ku_string_static((const uint8_t*)"cleanup failed", 14)}};
  }
  CHECK(ku_test_event_set(&r->finished)); return UINT32_MAX;
}
static uint32_t fixture_resume(void* raw) {
  FixtureRole* r = fixture_role(raw); KuTaskInstance_0* instance = (KuTaskInstance_0*)raw; fixture_inc(&r->resumes);
  if (r->entry_gate) { r->entry_gate = 0; CHECK(ku_test_event_set(&r->entered)); CHECK(ku_test_event_wait(&r->proceed, 2000)); }
  if (r->mode == MODE_PROBE && !r->done) { fixture_probe(r, instance); r->done = 1; CHECK(ku_test_event_set(&r->finished)); }
  if (r->mode == MODE_WAIT && !r->done) return fixture_wait_step(r, instance, 0);
  if (r->mode == MODE_HOLD) { CHECK(ku_task_driver_set_intent(&instance->ticket, KU_TASK_DRIVER_WAIT) == KU_TASK_DRIVER_OK); return KU_TASK_CONTROL_PENDING; }
  return UINT32_MAX;
}
static uint32_t fixture_cleanup(void* raw, uint32_t reason) {
  FixtureRole* r = fixture_role(raw); KuTaskInstance_0* instance = (KuTaskInstance_0*)raw;
  fixture_inc(&r->cleanups); r->reason = reason; CHECK(ku_test_event_set(&r->cleanup_entered));
  if (r->cleanup_hold) { CHECK(ku_task_driver_set_intent(&instance->ticket, KU_TASK_DRIVER_WAIT) == KU_TASK_DRIVER_OK); return KU_TASK_CONTROL_PENDING; }
  if (r->mode == MODE_PROBE && !r->done) { fixture_probe(r, instance); r->done = 1; CHECK(ku_test_event_set(&r->finished)); }
  if (r->wait_cleanup && !r->done) return fixture_wait_step(r, instance, 1);
  if (r->wait.driver) CHECK(ku_task_driver_wait_detach(&r->wait) == KU_TASK_DRIVER_OK);
  return UINT32_MAX;
}
static void fixture_drop_frame(void* raw) { fixture_inc(&fixture_role(raw)->frame_drops); }
static void fixture_drop_payload(void* raw) { fixture_inc(&fixture_role(raw)->payload_drops); }
static uint32_t fixture_take(void* raw) {
  FixtureRole* r = fixture_role(raw);
  if (r->take_gate) { CHECK(ku_test_event_set(&r->take_entered)); CHECK(ku_test_event_wait(&r->take_proceed, 2000)); if (r->take_reject) return KU_TASK_CONTROL_INVALID_ARGUMENT; }
  return UINT32_MAX;
}
static void fixture_after_store(void* raw) {
  FixtureRole* r = fixture_role(((KuTaskControlV1*)raw)->context);
  if (r->store_gate) { CHECK(ku_test_event_set(&r->stored)); CHECK(ku_test_event_wait(&r->store_proceed, 2000)); }
}
static void fixture_publishing(void* raw) {
  FixtureRole* r = fixture_role(((KuTaskControlV1*)raw)->context);
  if (r->publishing_gate) { CHECK(ku_test_event_set(&r->publishing)); CHECK(ku_test_event_wait(&r->publish_proceed, 2000)); }
}
static void fixture_before_ack(void* raw) {
  KuTaskControlV1* c = (KuTaskControlV1*)raw;
  if (fixture_role(c->context)->corrupt_ack) { CHECK(c->frame_destroyed && !fixture_count(&c->lifecycle_pin)); c->frame_destroyed = 0; }
}
static void fixture_ack(void* raw) {
  FixtureRole* r = fixture_role(((KuTaskControlV1*)raw)->context); fixture_inc(&r->acks);
  CHECK(ku_test_event_set(&r->acked)); /* Witness under queue lock, not a production callback. */
}
static void* fixture_before_dispose(void* raw) { return fixture_role(raw); }
static void fixture_disposed(void* raw) { FixtureRole* r = (FixtureRole*)raw; fixture_inc(&r->disposes); CHECK(ku_test_event_set(&r->disposed)); }

static void fixture_pending_child(FixtureDriver* runtime, size_t i) {
  fixture_role_init(i, MODE_HOLD); roles[i].cleanup_hold = 1;
  fixture_start(runtime, i); fixture_idle(runtime, 0);
  fixture_transfer(i, fixture_deadline()); fixture_idle(runtime, 0);
  CHECK(ku_task_driver_cleanup_receipt_read(&roles[i].receipt) == KU_TASK_DRIVER_PENDING);
}
static void fixture_wait_parent(FixtureDriver* runtime, size_t i, size_t child, uint64_t deadline) {
  fixture_role_init(i, MODE_WAIT); roles[i].target = child; roles[i].target_receipt = roles[child].receipt;
  roles[i].deadline = deadline; fixture_start(runtime, i);
}
static void fixture_completed_parent(FixtureDriver* runtime, size_t i, uint32_t expected, uint32_t fault) {
  CHECK(ku_test_event_wait(&roles[i].finished, 2000)); fixture_idle(runtime, fault);
  CHECK(roles[i].outcome == expected && fixture_count(&roles[i].continuations) == 1);
  CHECK(ku_task_control_atomic_load(&roles[i].handle.owner.lease.control->phase) == (expected == KU_TASK_DRIVER_CLEANUP_ACK ? KU_TASK_CONTROL_COMPLETED : KU_TASK_CONTROL_FAILED));
  KuTaskAdapterOutcomeV1 value = {0}; KuTaskAdapterTakeRequestV1 request = {&value, NULL};
  CHECK(ku_task_driver_take_result(&roles[i].ticket, &roles[i].handle.owner.lease, &request) == KU_TASK_CONTROL_OK);
  CHECK(value.result_kind == 4u && value.exit_class == KU_TASK_EXIT_USER_RESULT);
  CHECK(!value.has_cleanup_deadline && !value.cleanup_deadline);
  CHECK(value.value.string.ok == (expected == KU_TASK_DRIVER_CLEANUP_ACK));
  if (value.value.string.ok) CHECK(value.value.string.value.len == 7 && !memcmp(value.value.string.value.ptr, "payload", 7));
  else if (expected == KU_TASK_DRIVER_CLEANUP_TIMEOUT) CHECK(value.value.string.error.code.len == 16 && !memcmp(value.value.string.error.code.ptr, "shutdown_timeout", 16));
  else CHECK(value.value.string.error.code.len == 21 && !memcmp(value.value.string.error.code.ptr, "fixture_cleanup_fault", 21));
  ku_task_outcome_drop(&value); fixture_transfer(i, fixture_deadline()); fixture_disposed_wait(i);
}
static void fixture_stable(FixtureDriver* runtime, uint32_t fault) {
  fixture_idle(runtime, fault); KuTaskDriverSnapshotV1 before = fixture_snapshot(runtime, fault);
  for (size_t i = 0; i < 8; ++i) {
    fixture_idle(runtime, fault); KuTaskDriverSnapshotV1 after = fixture_snapshot(runtime, fault);
    CHECK(after.polls == before.polls && after.queued == before.queued && after.running == before.running);
  }
}
static void fixture_basic_wait(int cancel_first, int running_window) {
  fixture_clock_begin(); FixtureDriver runtime; fixture_driver_init(&runtime, 3);
  fixture_pending_child(&runtime, 0);
  KuTaskControlLeaseV1 observer = {0};
  CHECK(!ku_task_driver_lock(runtime.driver));
  CHECK(ku_task_control_lease_retain(&runtime.slots[roles[0].ticket.slot].driver_lease, &observer) == KU_TASK_CONTROL_OK);
  size_t child_charge = runtime.slots[roles[0].ticket.slot].charged_bytes;
  CHECK(!ku_task_driver_unlock(runtime.driver));
  if (cancel_first) {
    fixture_role_init(1, MODE_HOLD); roles[1].target_receipt = roles[0].receipt;
    roles[1].deadline = fixture_deadline(); roles[1].wait_cleanup = 1;
    fixture_start(&runtime, 1); fixture_idle(&runtime, 0);
    CHECK(ku_task_driver_request_cancel(&roles[1].ticket, &roles[1].handle.owner.lease,
        KU_TASK_CONTROL_TIMED_OUT, roles[1].deadline) == KU_TASK_CONTROL_OK);
  } else {
    fixture_role_init(1, MODE_WAIT); roles[1].target_receipt = roles[0].receipt; roles[1].deadline = fixture_deadline();
    roles[1].after_arm_gate = running_window; fixture_start(&runtime, 1);
  }
  if (running_window) {
    CHECK(ku_test_event_wait(&roles[1].entered, 2000));
    CHECK(fixture_wait_snapshot(1).state == KU_TASK_DRIVER_WAIT_ARMED);
    CHECK(!ku_task_driver_lock(runtime.driver));
    CHECK(runtime.slots[roles[1].ticket.slot].state == KU_TASK_DRIVER_RUNNING);
    CHECK(!ku_task_driver_unlock(runtime.driver));
    /* Child's real owner/cleanup quantum is queued while parent is RUNNING. */
    fixture_child_release(&runtime, 0); CHECK(ku_test_event_set(&roles[1].proceed));
  } else {
    fixture_armed(&runtime, 1); fixture_stable(&runtime, 0);
    KuTaskControlV1* parent = roles[1].handle.owner.lease.control;
    CHECK(parent && fixture_count(&parent->lifecycle_pin) == 1 && !parent->frame_destroyed);
    CHECK(((KuTaskInstance_0*)parent->context)->frame.s_1.value.ptr);
    if (cancel_first) {
      KuTaskDriverWaitTokenV1 original = roles[1].wait;
      CHECK(ku_task_driver_request_cancel(&roles[1].ticket, &roles[1].handle.owner.lease,
          KU_TASK_CONTROL_CANCELLED, roles[1].deadline + 1000u) == KU_TASK_CONTROL_OK);
      fixture_transfer(1, roles[1].deadline); fixture_idle(&runtime, 0);
      CHECK(roles[1].wait.epoch == original.epoch && roles[1].wait.parent_generation == original.parent_generation);
      CHECK(fixture_wait_snapshot(1).state == KU_TASK_DRIVER_WAIT_ARMED);
      /* A grandparent ACK waits on the cancelled parent's true cleanup. */
      fixture_wait_parent(&runtime, 2, 1, roles[1].deadline); fixture_armed(&runtime, 2);
      CHECK(!ku_task_driver_lock(runtime.driver));
      CHECK(runtime.slots[roles[1].ticket.slot].waiter_epoch == roles[2].wait.epoch);
      CHECK(!ku_task_driver_unlock(runtime.driver));
    }
    fixture_child_release(&runtime, 0);
  }
  CHECK(ku_test_event_wait(&roles[0].acked, 2000));
  if (cancel_first) {
    fixture_disposed_wait(1); fixture_idle(&runtime, 0);
    CHECK(roles[1].reason == KU_TASK_CONTROL_TIMED_OUT && fixture_count(&roles[1].resumes) == 1);
    CHECK(roles[1].outcome == KU_TASK_DRIVER_CLEANUP_ACK && fixture_count(&roles[1].continuations) == 1);
    fixture_completed_parent(&runtime, 2, KU_TASK_DRIVER_CLEANUP_ACK, 0);
  } else fixture_completed_parent(&runtime, 1, KU_TASK_DRIVER_CLEANUP_ACK, 0);
  CHECK(!fixture_count(&roles[0].disposes));
  CHECK(observer.control->frame_destroyed && !fixture_count(&observer.control->lifecycle_pin));
  KuTaskDriverSnapshotV1 held = fixture_snapshot(&runtime, 0);
  CHECK(held.resident == 1 && held.reserved_bytes == child_charge && held.retiring == 1);
  CHECK(ku_task_control_lease_release(&observer) == KU_TASK_CONTROL_OK);
  fixture_disposed_wait(0); fixture_finish(&runtime, 0);
}
static void fixture_old_receipt(void) {
  fixture_clock_begin(); FixtureDriver runtime; fixture_driver_init(&runtime, 1);
  fixture_role_init(0, MODE_FINISH); fixture_start(&runtime, 0); fixture_idle(&runtime, 0);
  fixture_transfer(0, fixture_deadline()); fixture_disposed_wait(0); fixture_idle(&runtime, 0);
  fixture_wait_parent(&runtime, 1, 0, fixture_deadline());
  CHECK(ku_test_event_wait(&roles[1].finished, 2000)); fixture_idle(&runtime, 0);
  CHECK(roles[1].ticket.slot == roles[0].ticket.slot && roles[1].ticket.generation > roles[0].ticket.generation);
  CHECK(roles[1].arm_status == KU_TASK_DRIVER_CLEANUP_ACK && !roles[1].wait.driver);
  fixture_completed_parent(&runtime, 1, KU_TASK_DRIVER_CLEANUP_ACK, 0); fixture_finish(&runtime, 0);
}
static void fixture_timeout_and_sticky(void) {
  fixture_clock_begin(); FixtureDriver runtime; fixture_driver_init(&runtime, 2); fixture_pending_child(&runtime, 0);
  uint64_t deadline = ku_task_driver_now_ms() + 100u; fixture_wait_parent(&runtime, 1, 0, deadline); fixture_armed(&runtime, 1);
  CHECK(ku_task_control_atomic_load(&roles[1].handle.owner.lease.control->phase) == KU_TASK_CONTROL_LIVE);
  fixture_advance(&runtime, deadline); CHECK(ku_test_event_wait(&roles[1].finished, 2000)); fixture_idle(&runtime, 0);
  CHECK(roles[1].outcome == KU_TASK_DRIVER_CLEANUP_TIMEOUT);
  CHECK(!fixture_count(&roles[1].cleanups)); /* Driver did not cancel normal parent. */
  CHECK(!ku_task_driver_lock(runtime.driver));
  KuTaskDriverSlotV1* parent = &runtime.slots[roles[1].ticket.slot];
  CHECK(parent->scope_wait_failure == KU_TASK_DRIVER_CLEANUP_TIMEOUT && parent->scope_wait_deadline == deadline);
  CHECK(!ku_task_driver_unlock(runtime.driver));
  fixture_child_release(&runtime, 0); fixture_disposed_wait(0); fixture_idle(&runtime, 0);
  CHECK(roles[1].outcome == KU_TASK_DRIVER_CLEANUP_TIMEOUT && fixture_count(&roles[1].continuations) == 1);
  fixture_completed_parent(&runtime, 1, KU_TASK_DRIVER_CLEANUP_TIMEOUT, 0); fixture_finish(&runtime, 0);
}
static void fixture_between_siblings(int notified, int second_pending) {
  fixture_clock_begin(); FixtureDriver runtime; fixture_driver_init(&runtime, 3);
  if (notified) fixture_pending_child(&runtime, 0);
  else { fixture_role_init(0, MODE_FINISH); fixture_start(&runtime, 0); fixture_idle(&runtime, 0); fixture_transfer(0, fixture_deadline()); fixture_disposed_wait(0); }
  if (second_pending) fixture_pending_child(&runtime, 1);
  else { fixture_role_init(1, MODE_FINISH); fixture_start(&runtime, 1); fixture_idle(&runtime, 0); fixture_transfer(1, fixture_deadline()); fixture_disposed_wait(1); }
  uint64_t deadline = ku_task_driver_now_ms() + 100u;
  fixture_role_init(2, notified ? MODE_WAIT : MODE_PROBE);
  roles[2].target_receipt = roles[0].receipt; roles[2].deadline = deadline; roles[2].probe = PROBE_FIRST_ACK; roles[2].hold_latch = notified;
  fixture_start(&runtime, 2);
  if (notified) {
    fixture_armed(&runtime, 2); fixture_child_release(&runtime, 0); fixture_disposed_wait(0); fixture_idle(&runtime, 0);
    CHECK(fixture_wait_snapshot(2).outcome == KU_TASK_DRIVER_CLEANUP_ACK);
  } else { CHECK(ku_test_event_wait(&roles[2].finished, 2000)); fixture_idle(&runtime, 0); CHECK(!roles[2].wait.driver); }
  fixture_advance(&runtime, deadline + 1u); fixture_idle(&runtime, 0);
  CHECK(!ku_task_driver_lock(runtime.driver));
  KuTaskDriverSlotV1* p = &runtime.slots[roles[2].ticket.slot];
  CHECK(p->scope_wait_started && p->scope_deadline_fired && !p->scope_wait_failure && p->scope_wait_deadline == deadline);
  roles[2].mode = MODE_HOLD; CHECK(!ku_task_driver_unlock(runtime.driver));
  if (notified) { CHECK(ku_task_driver_wait_detach(&roles[2].wait) == KU_TASK_DRIVER_OK); fixture_idle(&runtime, 0); }
  CHECK(!ku_task_driver_lock(runtime.driver));
  roles[2].target_receipt = roles[1].receipt; roles[2].deadline = deadline + 100000u;
  roles[2].done = 0; roles[2].hold_latch = 0; roles[2].mode = MODE_WAIT;
  CHECK(!ku_task_driver_unlock(runtime.driver)); CHECK(ku_task_driver_wake(&roles[2].ticket) == KU_TASK_DRIVER_OK);
  fixture_idle(&runtime, 0); uint32_t expected = second_pending ? KU_TASK_DRIVER_CLEANUP_TIMEOUT : KU_TASK_DRIVER_CLEANUP_ACK;
  CHECK(roles[2].arm_status == expected && roles[2].outcome == expected);
  CHECK(!ku_task_driver_lock(runtime.driver)); CHECK(p->scope_wait_deadline == deadline);
  CHECK(p->scope_wait_failure == (second_pending ? KU_TASK_DRIVER_CLEANUP_TIMEOUT : 0u));
  CHECK(!ku_task_driver_unlock(runtime.driver));
  if (second_pending) { fixture_child_release(&runtime, 1); fixture_disposed_wait(1); }
  fixture_completed_parent(&runtime, 2, expected, 0); fixture_finish(&runtime, 0);
}

static void fixture_probe(FixtureRole* r, KuTaskInstance_0* instance) {
  KuTaskDriverV1* driver = instance->ticket.driver;
  KuTaskDriverWaitTokenV1 token = {0}; size_t calls = fixture_ledger().calls;
  if (r->probe == PROBE_FIRST_ACK) {
    CHECK(ku_task_driver_cleanup_wait_arm(&instance->ticket, &r->target_receipt, r->deadline, &token) == KU_TASK_DRIVER_CLEANUP_ACK);
    CHECK(!token.driver); r->mode = MODE_HOLD; return;
  }
  if (r->probe == PROBE_SELF_ACK) {
    CHECK(ku_task_driver_cleanup_wait_arm(&instance->ticket, &r->receipt, r->deadline, &token) == KU_TASK_DRIVER_WAIT_CYCLE);
    CHECK(!token.driver && fixture_ledger().calls == calls);
    CHECK(!ku_task_driver_lock(driver));
    KuTaskDriverSlotV1* slot = &driver->slots[instance->ticket.slot];
    CHECK(!slot->waiter_epoch && !slot->wait_epoch && !slot->scope_wait_started);
    CHECK(!ku_task_driver_unlock(driver)); r->arm_status = KU_TASK_DRIVER_WAIT_CYCLE; return;
  }
  if (r->probe == PROBE_CYCLE_ACK) {
    /* This self target already has A's incoming edge, so occupied-pair
     * rejection precedes the general bounded cycle scan. */
    CHECK(ku_task_driver_cleanup_wait_arm(&instance->ticket, &r->receipt, r->deadline, &token) == KU_TASK_DRIVER_INVALID_STATE);
    CHECK(ku_task_driver_cleanup_wait_arm(&instance->ticket, &r->target_receipt, r->deadline, &token) == KU_TASK_DRIVER_WAIT_CYCLE);
    CHECK(!token.driver); r->arm_status = KU_TASK_DRIVER_WAIT_CYCLE; return;
  }
  if (r->probe == PROBE_FAILURE) {
    CHECK(ku_task_driver_cleanup_wait_arm(&instance->ticket, &r->target_receipt, 0, &token) == KU_TASK_DRIVER_CLEANUP_TIMEOUT);
    CHECK(ku_task_driver_cleanup_wait_arm(&instance->ticket, &roles[1].receipt, fixture_deadline(), &token) == KU_TASK_DRIVER_CLEANUP_TIMEOUT);
    CHECK(!token.driver); r->arm_status = KU_TASK_DRIVER_CLEANUP_TIMEOUT; return;
  }
  if (r->probe == PROBE_HEADERS) {
    KuTaskDriverTicketV1 ticket = instance->ticket;
    KuTaskDriverCleanupReceiptV1 receipt = r->target_receipt, bad = receipt;
    CHECK(ku_task_driver_cleanup_wait_arm(&instance->ticket, &receipt, r->deadline, NULL) == KU_TASK_DRIVER_INVALID_ARGUMENT);
    CHECK(ku_task_driver_cleanup_wait_arm(&instance->ticket, &receipt, UINT64_MAX, &token) == KU_TASK_DRIVER_INVALID_ARGUMENT);
    CHECK(ku_task_driver_cleanup_wait_arm(&instance->ticket, &receipt, r->deadline, (KuTaskDriverWaitTokenV1*)&instance->ticket) == KU_TASK_DRIVER_INVALID_ARGUMENT);
    CHECK(ku_task_driver_cleanup_wait_arm(&instance->ticket, &receipt, r->deadline, (KuTaskDriverWaitTokenV1*)&receipt) == KU_TASK_DRIVER_INVALID_ARGUMENT);
    CHECK(ku_task_driver_cleanup_wait_arm(&instance->ticket, &receipt, r->deadline, (KuTaskDriverWaitTokenV1*)&instance->control) == KU_TASK_DRIVER_INVALID_ARGUMENT);
    CHECK(ku_task_driver_cleanup_wait_arm(&instance->ticket, &receipt, r->deadline, (KuTaskDriverWaitTokenV1*)driver) == KU_TASK_DRIVER_INVALID_ARGUMENT);
    CHECK(ku_task_driver_cleanup_wait_arm(&instance->ticket, &receipt, r->deadline, (KuTaskDriverWaitTokenV1*)driver->slots) == KU_TASK_DRIVER_INVALID_ARGUMENT);
    CHECK(ku_task_driver_cleanup_wait_arm(&instance->ticket, &receipt, r->deadline, (KuTaskDriverWaitTokenV1*)driver->ring) == KU_TASK_DRIVER_INVALID_ARGUMENT);
    CHECK(ku_task_driver_cleanup_wait_arm(&instance->ticket, &receipt, r->deadline, (KuTaskDriverWaitTokenV1*)driver->slots[receipt.slot].binding) == KU_TASK_DRIVER_INVALID_ARGUMENT);
    bad.abi_version = 3; CHECK(ku_task_driver_cleanup_wait_arm(&instance->ticket, &bad, r->deadline, &token) == KU_TASK_DRIVER_ABI_MISMATCH);
    bad = receipt; bad.generation++; CHECK(ku_task_driver_cleanup_wait_arm(&instance->ticket, &bad, r->deadline, &token) == KU_TASK_DRIVER_INVALID_ARGUMENT);
    CHECK(ku_task_driver_cleanup_wait_arm(&instance->ticket, &roles[3].receipt, r->deadline, &token) == KU_TASK_DRIVER_INVALID_ARGUMENT);
    token.epoch = 1; CHECK(ku_task_driver_cleanup_wait_arm(&instance->ticket, &receipt, r->deadline, &token) == KU_TASK_DRIVER_INVALID_STATE); token = (KuTaskDriverWaitTokenV1){0};
    CHECK(!memcmp(&ticket, &instance->ticket, sizeof(ticket)) && !memcmp(&receipt, &r->target_receipt, sizeof(receipt)));
    CHECK(!ku_task_driver_lock(driver)); CHECK(!driver->slots[ticket.slot].scope_wait_started && !driver->slots[ticket.slot].wait_epoch);
    CHECK(!ku_task_driver_unlock(driver));
  }
  CHECK(ku_task_driver_cleanup_wait_arm(&instance->ticket, &r->target_receipt, r->deadline, &token) == KU_TASK_DRIVER_PENDING);
  KuTaskDriverWaitTokenV1 old = token; KuTaskDriverWaitSnapshotV1 s = {0};
  CHECK(ku_task_driver_wait_read(&token, &s) == KU_TASK_DRIVER_OK && s.kind == KU_TASK_DRIVER_WAIT_KIND_CLEANUP_ACK);
  CHECK(ku_task_driver_cleanup_wait_arm(&instance->ticket, &r->target_receipt, r->deadline + 1u, &token) == KU_TASK_DRIVER_INVALID_STATE);
  CHECK(ku_task_driver_wait_read(&token, (KuTaskDriverWaitSnapshotV1*)&token) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(ku_task_driver_wait_read(&token, (KuTaskDriverWaitSnapshotV1*)&instance->control) == KU_TASK_DRIVER_INVALID_ARGUMENT);
  CHECK(ku_task_driver_wait_detach(&token) == KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_cleanup_wait_arm(&instance->ticket, &r->target_receipt, r->deadline + 100000u, &token) == KU_TASK_DRIVER_PENDING);
  CHECK(token.epoch > old.epoch); CHECK(ku_task_driver_wait_detach(&old) == KU_TASK_DRIVER_STALE);
  s = (KuTaskDriverWaitSnapshotV1){91u, 92u, 93u, 94u, 95u};
  CHECK(ku_task_driver_wait_read(&old, &s) == KU_TASK_DRIVER_STALE);
  CHECK(s.state == 91u && s.outcome == 92u && s.child_slot == 93u && s.child_generation == 94u && s.kind == 95u);
  CHECK(!ku_task_driver_lock(driver)); CHECK(driver->slots[instance->ticket.slot].scope_wait_deadline == r->deadline);
  CHECK(!ku_task_driver_unlock(driver));
  CHECK(ku_task_driver_wait_detach(&token) == KU_TASK_DRIVER_OK);
  CHECK(!ku_task_driver_lock(driver)); driver->slots[instance->ticket.slot].wait_epoch = UINT64_MAX;
  CHECK(!ku_task_driver_unlock(driver));
  CHECK(ku_task_driver_cleanup_wait_arm(&instance->ticket, &r->target_receipt, r->deadline, &token) == KU_TASK_DRIVER_LIMIT);
  CHECK(!token.driver && fixture_ledger().calls == calls); r->arm_status = KU_TASK_DRIVER_OK;
}
static void fixture_preflight_and_epoch(void) {
  fixture_clock_begin(); FixtureDriver runtime, other; fixture_driver_init(&runtime, 4); fixture_driver_init(&other, 1);
  fixture_pending_child(&runtime, 0);
  fixture_role_init(3, MODE_FINISH); fixture_start(&other, 3); fixture_idle(&other, 0);
  fixture_transfer(3, fixture_deadline()); fixture_disposed_wait(3); fixture_idle(&other, 0);
  fixture_role_init(1, MODE_PROBE); roles[1].probe = PROBE_HEADERS; roles[1].target_receipt = roles[0].receipt; roles[1].deadline = fixture_deadline();
  fixture_start(&runtime, 1); CHECK(ku_test_event_wait(&roles[1].finished, 2000)); fixture_idle(&runtime, 0);
  CHECK(roles[1].arm_status == KU_TASK_DRIVER_OK); fixture_transfer(1, fixture_deadline()); fixture_disposed_wait(1);
  fixture_child_release(&runtime, 0); fixture_disposed_wait(0);
  CHECK(ku_task_driver_shutdown(other.driver, fixture_deadline()) == KU_TASK_DRIVER_OK);
  uint32_t result = KU_TASK_DRIVER_PENDING; uint64_t end = ku_test_real_now_ms() + 2000u;
  for (size_t i = 0; i < 4096 && result == KU_TASK_DRIVER_PENDING; ++i) {
    result = ku_task_driver_destroy(other.driver);
    if (result == KU_TASK_DRIVER_PENDING) { CHECK(ku_test_real_now_ms() < end); ku_test_thread_yield(); }
  }
  CHECK(result == KU_TASK_DRIVER_OK); free(other.ring); free(other.slots); free(other.driver);
  fixture_finish(&runtime, 0);
}
static void fixture_self_cycle(void) {
  fixture_clock_begin(); FixtureDriver runtime; fixture_driver_init(&runtime, 1);
  fixture_pending_child(&runtime, 0);
  CHECK(!ku_task_driver_lock(runtime.driver));
  roles[0].cleanup_hold = 0; roles[0].mode = MODE_PROBE;
  roles[0].probe = PROBE_SELF_ACK; roles[0].deadline = fixture_deadline();
  CHECK(!ku_task_driver_unlock(runtime.driver));
  CHECK(ku_task_driver_wake(&roles[0].ticket) == KU_TASK_DRIVER_OK);
  fixture_disposed_wait(0); fixture_idle(&runtime, 0);
  CHECK(roles[0].arm_status == KU_TASK_DRIVER_WAIT_CYCLE); fixture_finish(&runtime, 0);
}
static void fixture_cycle(void) {
  fixture_clock_begin(); FixtureDriver runtime; fixture_driver_init(&runtime, 2);
  fixture_pending_child(&runtime, 0); fixture_pending_child(&runtime, 1);
  CHECK(!ku_task_driver_lock(runtime.driver));
  roles[0].cleanup_hold = 0; roles[0].wait_cleanup = 1; roles[0].target_receipt = roles[1].receipt; roles[0].deadline = fixture_deadline();
  CHECK(!ku_task_driver_unlock(runtime.driver)); CHECK(ku_task_driver_wake(&roles[0].ticket) == KU_TASK_DRIVER_OK); fixture_armed(&runtime, 0);
  CHECK(!ku_task_driver_lock(runtime.driver));
  roles[1].cleanup_hold = 0; roles[1].mode = MODE_PROBE; roles[1].probe = PROBE_CYCLE_ACK;
  roles[1].target_receipt = roles[0].receipt; roles[1].deadline = fixture_deadline();
  CHECK(!ku_task_driver_unlock(runtime.driver)); CHECK(ku_task_driver_wake(&roles[1].ticket) == KU_TASK_DRIVER_OK);
  fixture_disposed_wait(1); fixture_disposed_wait(0); fixture_idle(&runtime, 0);
  CHECK(roles[1].arm_status == KU_TASK_DRIVER_WAIT_CYCLE && roles[0].outcome == KU_TASK_DRIVER_CLEANUP_ACK);
  fixture_finish(&runtime, 0);
}
static void fixture_failure_rearm(void) {
  fixture_clock_begin(); FixtureDriver runtime; fixture_driver_init(&runtime, 3); fixture_pending_child(&runtime, 0);
  fixture_role_init(1, MODE_FINISH); fixture_start(&runtime, 1); fixture_idle(&runtime, 0); fixture_transfer(1, fixture_deadline()); fixture_disposed_wait(1);
  fixture_role_init(2, MODE_PROBE); roles[2].probe = PROBE_FAILURE; roles[2].target_receipt = roles[0].receipt;
  fixture_start(&runtime, 2); CHECK(ku_test_event_wait(&roles[2].finished, 2000)); fixture_idle(&runtime, 0);
  CHECK(roles[2].arm_status == KU_TASK_DRIVER_CLEANUP_TIMEOUT); fixture_transfer(2, fixture_deadline()); fixture_disposed_wait(2);
  fixture_child_release(&runtime, 0); fixture_disposed_wait(0); fixture_finish(&runtime, 0);
}
static void fixture_real_timer(void) {
  CHECK(!ku_task_control_deadline_load(&fixture_clock)); FixtureDriver runtime; fixture_driver_init(&runtime, 2);
  fixture_pending_child(&runtime, 0);
  fixture_wait_parent(&runtime, 1, 0, ku_task_driver_now_ms() + 100u);
  /* No clock advance, unrelated wake, repeated polling, or sleep: the real
   * condition deadline must resume the parent before this real event limit. */
  CHECK(ku_test_event_wait(&roles[1].finished, 2000)); fixture_idle(&runtime, 0);
  CHECK(roles[1].outcome == KU_TASK_DRIVER_CLEANUP_TIMEOUT && !fixture_count(&roles[1].cleanups));
  fixture_completed_parent(&runtime, 1, KU_TASK_DRIVER_CLEANUP_TIMEOUT, 0);
  fixture_child_release(&runtime, 0); fixture_disposed_wait(0); fixture_finish(&runtime, 0);
}

typedef struct FixtureTake { KuTaskDriverTicketV1 ticket; KuTaskControlLeaseV1 lease; KuTaskAdapterOutcomeV1 value; uint32_t status; } FixtureTake;
static int fixture_take_thread(void* raw) {
  FixtureTake* take = (FixtureTake*)raw;
  KuTaskAdapterTakeRequestV1 request = {&take->value, NULL};
  take->status = ku_task_driver_take_result(&take->ticket, &take->lease, &request); return 0;
}
static void fixture_taking(int reject) {
  fixture_clock_begin(); FixtureDriver runtime; fixture_driver_init(&runtime, 2);
  fixture_role_init(0, MODE_FINISH); roles[0].take_gate = 1; roles[0].store_gate = 1; roles[0].take_reject = reject;
  fixture_start(&runtime, 0); fixture_idle(&runtime, 0);
  FixtureTake take = {0}; take.ticket = roles[0].ticket;
  CHECK(ku_task_control_lease_retain(&roles[0].handle.owner.lease, &take.lease) == KU_TASK_CONTROL_OK);
  KuTestThread thread; CHECK(ku_test_thread_start(&thread, fixture_take_thread, &take));
  CHECK(ku_test_event_wait(&roles[0].take_entered, 2000));
  fixture_transfer(0, fixture_deadline()); fixture_idle(&runtime, 0);
  CHECK(!ku_task_driver_lock(runtime.driver));
  CHECK(runtime.slots[roles[0].ticket.slot].owner_location == KU_TASK_DRIVER_OWNER_DEFERRED);
  CHECK(!ku_task_driver_unlock(runtime.driver));
  fixture_wait_parent(&runtime, 1, 0, fixture_deadline()); fixture_armed(&runtime, 1); fixture_stable(&runtime, 0);
  CHECK(ku_test_event_set(&roles[0].take_proceed)); CHECK(ku_test_event_wait(&roles[0].stored, 2000));
  CHECK(fixture_wait_snapshot(1).state == KU_TASK_DRIVER_WAIT_ARMED && !fixture_count(&roles[0].acks));
  CHECK(ku_test_event_set(&roles[0].store_proceed)); CHECK(ku_test_thread_join(&thread, 2000) && !thread.outcome);
  CHECK(take.status == (reject ? KU_TASK_CONTROL_INVALID_ARGUMENT : KU_TASK_CONTROL_OK));
  fixture_completed_parent(&runtime, 1, KU_TASK_DRIVER_CLEANUP_ACK, 0);
  CHECK(!fixture_count(&roles[0].disposes)); CHECK(fixture_count(&roles[0].payload_drops) == (reject ? 1u : 0u));
  CHECK(ku_task_control_lease_release(&take.lease) == KU_TASK_CONTROL_OK); ku_task_outcome_drop(&take.value);
  fixture_disposed_wait(0); fixture_finish(&runtime, 0);
}
static void fixture_full_resources(void) {
  fixture_clock_begin(); FixtureDriver runtime; fixture_driver_init(&runtime, 4); fixture_pending_child(&runtime, 0);
  KuTaskControlLeaseV1 observer = {0}; CHECK(!ku_task_driver_lock(runtime.driver));
  CHECK(ku_task_control_lease_retain(&runtime.slots[roles[0].ticket.slot].driver_lease, &observer) == KU_TASK_CONTROL_OK);
  CHECK(!ku_task_driver_unlock(runtime.driver));
  size_t count = KU_TASK_CONTROL_MAX_REFERENCES - fixture_count(&observer.control->references);
  KuTaskControlLeaseV1* held = (KuTaskControlLeaseV1*)calloc(count, sizeof(*held)); CHECK(held && count);
  for (size_t i = 0; i < count; ++i) CHECK(ku_task_control_lease_retain(&observer, &held[i]) == KU_TASK_CONTROL_OK);
  CHECK(fixture_count(&observer.control->references) == KU_TASK_CONTROL_MAX_REFERENCES);
  fixture_role_init(1, MODE_WAIT); roles[1].target_receipt = roles[0].receipt; roles[1].deadline = fixture_deadline();
  roles[1].entry_gate = 1; roles[1].queue_full = 1; fixture_start(&runtime, 1);
  CHECK(ku_test_event_wait(&roles[1].entered, 2000));
  for (size_t i = 2; i < 4; ++i) { fixture_role_init(i, MODE_FINISH); fixture_start(&runtime, i); }
  CHECK(ku_task_driver_wake(&roles[0].ticket) == KU_TASK_DRIVER_OK);
  CHECK(!ku_task_driver_lock(runtime.driver)); CHECK(runtime.driver->queued == 3 && runtime.driver->resident == 4);
  CHECK(!ku_task_driver_unlock(runtime.driver)); CHECK(ku_test_event_set(&roles[1].proceed));
  fixture_armed(&runtime, 1); CHECK(fixture_count(&observer.control->references) == KU_TASK_CONTROL_MAX_REFERENCES);
  fixture_child_release(&runtime, 0); fixture_completed_parent(&runtime, 1, KU_TASK_DRIVER_CLEANUP_ACK, 0);
  for (size_t i = 2; i < 4; ++i) { fixture_transfer(i, fixture_deadline()); fixture_disposed_wait(i); }
  CHECK(fixture_count(&observer.control->references) == count + 1u && !fixture_count(&roles[0].disposes));
  for (size_t i = 0; i < count; ++i) CHECK(ku_task_control_lease_release(&held[i]) == KU_TASK_CONTROL_OK);
  free(held); CHECK(ku_task_control_lease_release(&observer) == KU_TASK_CONTROL_OK);
  fixture_disposed_wait(0); fixture_finish(&runtime, 0);
}
typedef struct FixtureCancel { KuTaskDriverTicketV1 ticket; KuTaskControlLeaseV1 lease; uint64_t deadline; uint32_t status; } FixtureCancel;
static int fixture_cancel_thread(void* raw) {
  FixtureCancel* cancel = (FixtureCancel*)raw;
  cancel->status = ku_task_driver_request_cancel(&cancel->ticket, &cancel->lease, KU_TASK_CONTROL_CANCELLED, cancel->deadline); return 0;
}
static void fixture_publishing_parent(void) {
  fixture_clock_begin(); FixtureDriver runtime; fixture_driver_init(&runtime, 2); fixture_pending_child(&runtime, 0);
  fixture_role_init(1, MODE_WAIT); roles[1].target_receipt = roles[0].receipt; roles[1].deadline = fixture_deadline();
  roles[1].entry_gate = 1; roles[1].publishing_gate = 1; roles[1].wait_cleanup = 1;
  fixture_start(&runtime, 1); CHECK(ku_test_event_wait(&roles[1].entered, 2000));
  FixtureCancel cancel = {0}; cancel.ticket = roles[1].ticket; cancel.deadline = 0;
  CHECK(ku_task_control_lease_retain(&roles[1].handle.owner.lease, &cancel.lease) == KU_TASK_CONTROL_OK);
  KuTestThread thread; CHECK(ku_test_thread_start(&thread, fixture_cancel_thread, &cancel));
  CHECK(ku_test_event_wait(&roles[1].publishing, 2000));
  CHECK(ku_task_control_atomic_load(&cancel.lease.control->phase) == KU_TASK_CONTROL_PUBLISHING_CANCEL);
  CHECK(ku_test_event_set(&roles[1].proceed)); fixture_armed(&runtime, 1);
  CHECK(!ku_task_driver_lock(runtime.driver));
  CHECK(runtime.slots[roles[1].ticket.slot].scope_wait_deadline == roles[1].deadline);
  CHECK(!ku_task_driver_unlock(runtime.driver));
  CHECK(ku_test_event_set(&roles[1].publish_proceed)); CHECK(ku_test_thread_join(&thread, 2000) && !thread.outcome);
  CHECK(cancel.status == KU_TASK_CONTROL_OK); CHECK(ku_test_event_wait(&roles[1].finished, 2000)); fixture_idle(&runtime, 0);
  CHECK(roles[1].outcome == KU_TASK_DRIVER_CLEANUP_TIMEOUT && roles[1].reason == KU_TASK_CONTROL_CANCELLED);
  CHECK(fixture_count(&roles[1].resumes) == 1 && fixture_count(&roles[1].continuations) == 1);
  CHECK(ku_task_control_atomic_load(&cancel.lease.control->phase) == KU_TASK_CONTROL_CANCELLED);
  CHECK(ku_task_control_lease_release(&cancel.lease) == KU_TASK_CONTROL_OK);
  fixture_transfer(1, 0); fixture_disposed_wait(1); fixture_child_release(&runtime, 0); fixture_disposed_wait(0);
  fixture_finish(&runtime, 0);
}
static void fixture_shutdown_tightens(void) {
  fixture_clock_begin(); FixtureDriver runtime; fixture_driver_init(&runtime, 2); fixture_pending_child(&runtime, 0);
  fixture_role_init(1, MODE_WAIT); roles[1].target_receipt = roles[0].receipt; roles[1].deadline = fixture_deadline(); roles[1].wait_cleanup = 1;
  fixture_start(&runtime, 1); fixture_armed(&runtime, 1); KuTaskDriverWaitTokenV1 old = roles[1].wait;
  CHECK(ku_task_driver_shutdown(runtime.driver, 0) == KU_TASK_DRIVER_SHUTDOWN_TIMEOUT);
  CHECK(ku_test_event_wait(&roles[1].finished, 2000)); fixture_idle(&runtime, 0);
  CHECK(roles[1].outcome == KU_TASK_DRIVER_CLEANUP_TIMEOUT && roles[1].old_wait.epoch == old.epoch);
  CHECK(roles[1].reason == KU_TASK_CONTROL_CANCELLED && fixture_count(&roles[1].resumes) == 1);
  CHECK(!ku_task_driver_lock(runtime.driver));
  CHECK(runtime.slots[roles[1].ticket.slot].scope_wait_deadline == 0 && runtime.slots[roles[1].ticket.slot].scope_wait_failure == KU_TASK_DRIVER_CLEANUP_TIMEOUT);
  CHECK(!ku_task_driver_unlock(runtime.driver));
  fixture_transfer(1, 0); fixture_disposed_wait(1); fixture_child_release(&runtime, 0); fixture_disposed_wait(0);
  fixture_idle(&runtime, 0); fixture_finish(&runtime, 0);
}
static void fixture_clock_fault(int acknowledged) {
  fixture_clock_begin(); FixtureDriver runtime; fixture_driver_init(&runtime, 2);
  if (acknowledged) {
    fixture_role_init(0, MODE_FINISH); fixture_start(&runtime, 0); fixture_idle(&runtime, 0);
    fixture_transfer(0, fixture_deadline()); fixture_disposed_wait(0);
  } else fixture_pending_child(&runtime, 0);
  fixture_role_init(1, MODE_HOLD); roles[1].target_receipt = roles[0].receipt;
  roles[1].deadline = fixture_deadline(); roles[1].wait_cleanup = 1;
  fixture_start(&runtime, 1); fixture_idle(&runtime, 0);
  ku_task_control_deadline_store(&fixture_clock, UINT64_MAX);
  CHECK(ku_task_driver_shutdown(runtime.driver, 0) == KU_TASK_DRIVER_INTERNAL);
  CHECK(ku_test_event_wait(&roles[1].finished, 2000));
  CHECK(roles[1].outcome == (acknowledged ? KU_TASK_DRIVER_CLEANUP_ACK : KU_TASK_DRIVER_CLEANUP_TIMEOUT));
  CHECK(roles[1].reason == KU_TASK_CONTROL_CANCELLED && !roles[1].wait.driver);
  fixture_transfer(1, 0); fixture_disposed_wait(1);
  if (!acknowledged) { fixture_child_release(&runtime, 0); fixture_disposed_wait(0); }
  fixture_finish(&runtime, KU_TASK_DRIVER_INTERNAL);
}
static void fixture_damaged_pair(void) {
  fixture_clock_begin(); FixtureDriver runtime; fixture_driver_init(&runtime, 4);
  fixture_pending_child(&runtime, 0); fixture_pending_child(&runtime, 1);
  uint64_t deadline = ku_task_driver_now_ms() + 100u;
  fixture_wait_parent(&runtime, 2, 0, deadline); fixture_armed(&runtime, 2);
  fixture_wait_parent(&runtime, 3, 1, deadline + 10000u); fixture_armed(&runtime, 3);
  CHECK(!ku_task_driver_lock(runtime.driver));
  KuTaskDriverSlotV1* a = &runtime.slots[roles[2].ticket.slot];
  KuTaskDriverSlotV1* b = &runtime.slots[roles[3].ticket.slot];
  KuTaskDriverSlotV1* child_b = &runtime.slots[roles[1].ticket.slot];
  /* A's outgoing metadata alone is damaged. B's real registration and its
   * reciprocal child are valid and must never receive A's expired timeout. */
  a->wait_child_slot = roles[1].ticket.slot; a->wait_child_generation = roles[1].ticket.generation;
  uint64_t b_epoch = b->wait_epoch; CHECK(child_b->waiter_epoch == b_epoch);
  CHECK(!ku_task_driver_unlock(runtime.driver)); fixture_advance(&runtime, deadline);
  CHECK(ku_test_event_wait(&roles[2].finished, 2000)); fixture_idle(&runtime, KU_TASK_DRIVER_INTERNAL);
  CHECK(roles[2].outcome == KU_TASK_DRIVER_INTERNAL);
  KuTaskDriverWaitSnapshotV1 intact = fixture_wait_snapshot(3);
  CHECK(intact.state == KU_TASK_DRIVER_WAIT_ARMED && intact.outcome == KU_TASK_DRIVER_PENDING);
  CHECK(!ku_task_driver_lock(runtime.driver));
  CHECK(b->wait_epoch == b_epoch && child_b->waiter_epoch == b_epoch && child_b->waiter_parent_slot == roles[3].ticket.slot);
  CHECK(!b->scope_wait_failure); CHECK(!ku_task_driver_unlock(runtime.driver));
  fixture_child_release(&runtime, 1); fixture_disposed_wait(1);
  fixture_completed_parent(&runtime, 3, KU_TASK_DRIVER_CLEANUP_ACK, KU_TASK_DRIVER_INTERNAL);
  /* Let the actual original child publish/clear its now-obsolete incoming
   * link; never repair metadata or clear sticky fault to fake recovery. */
  fixture_child_release(&runtime, 0); fixture_disposed_wait(0);
  fixture_completed_parent(&runtime, 2, KU_TASK_DRIVER_INTERNAL, KU_TASK_DRIVER_INTERNAL);
  fixture_finish(&runtime, KU_TASK_DRIVER_INTERNAL);
}
static void fixture_corrupt_ack_process(void) {
  fixture_clock_begin(); FixtureDriver* runtime = &fixture_quarantined_driver; fixture_driver_init(runtime, 2);
  fixture_role_init(0, MODE_HOLD); roles[0].cleanup_hold = 1; roles[0].corrupt_ack = 1;
  fixture_start(runtime, 0); fixture_idle(runtime, 0); fixture_transfer(0, fixture_deadline()); fixture_idle(runtime, 0);
  fixture_wait_parent(runtime, 1, 0, fixture_deadline()); fixture_armed(runtime, 1);
  CHECK(!ku_task_driver_lock(runtime->driver)); size_t charge = runtime->slots[roles[0].ticket.slot].charged_bytes;
  CHECK(!ku_task_driver_unlock(runtime->driver));
  fixture_child_release(runtime, 0); fixture_completed_parent(runtime, 1, KU_TASK_DRIVER_INTERNAL, KU_TASK_DRIVER_INTERNAL);
  fixture_idle(runtime, KU_TASK_DRIVER_INTERNAL);
  CHECK(ku_task_driver_cleanup_receipt_read(&roles[0].receipt) == KU_TASK_DRIVER_INTERNAL);
  KuTaskDriverSnapshotV1 before = fixture_snapshot(runtime, KU_TASK_DRIVER_INTERNAL);
  CHECK(before.resident == 1 && before.reserved_bytes == charge && before.terminal_held == 1);
  CHECK(!ku_task_driver_lock(runtime->driver)); KuTaskDriverSlotV1* child = &runtime->slots[roles[0].ticket.slot];
  CHECK(child->cleanup_fault == KU_TASK_DRIVER_INTERNAL && !child->cleanup_acked_generation && !child->waiter_epoch);
  CHECK(child->driver_lease.control && child->execution_lease.control && child->binding);
  CHECK(fixture_count(&child->binding->references) == 2 && !fixture_count(&child->binding->lifecycle_pin));
  CHECK(ku_task_driver_return_slot(runtime->driver, child) == KU_TASK_DRIVER_INTERNAL);
  CHECK(runtime->driver->resident == 1 && runtime->driver->reserved_bytes == charge);
  CHECK(!ku_task_driver_unlock(runtime->driver));
  for (size_t i = 0; i < 8; ++i) {
    CHECK(ku_task_driver_wake(&roles[0].ticket) == KU_TASK_DRIVER_INTERNAL); fixture_idle(runtime, KU_TASK_DRIVER_INTERNAL);
    CHECK(fixture_snapshot(runtime, KU_TASK_DRIVER_INTERNAL).polls == before.polls);
  }
  CHECK(!fixture_count(&roles[0].acks) && !fixture_count(&roles[0].disposes));
  CHECK(fixture_count(&roles[1].continuations) == 1 && fixture_count(&roles[1].disposes) == 1);
  CHECK(ku_task_driver_shutdown(runtime->driver, 0) == KU_TASK_DRIVER_SHUTDOWN_TIMEOUT);
  CHECK(ku_task_driver_destroy(runtime->driver) == KU_TASK_DRIVER_PENDING);
  CHECK(fixture_ledger().allocations == 4 && fixture_ledger().bytes != 0);
  /* Normal return with sanitizer handlers intact. The rooted quarantine is
   * NOT ledger-zero or leak-free cleanup; only process isolation reclaims it. */
  puts("task-cleanup-wait-quarantined-not-drained");
}
int main(int argc, char** argv) {
  CHECK(KU_TASK_DRIVER_ABI_VERSION == 5u); CHECK(KU_TASK_FRAME_ABI_VERSION == 2u); ku_task_control_deadline_init(&fixture_clock);
  if (argc == 2 && !strcmp(argv[1], "--corrupt-ack")) { fixture_corrupt_ack_process(); return 0; }
  CHECK(argc == 1);
  fixture_basic_wait(0, 0); fixture_basic_wait(0, 1); fixture_basic_wait(1, 0);
  fixture_old_receipt(); fixture_timeout_and_sticky();
  for (int notified = 0; notified < 2; ++notified)
    for (int pending = 0; pending < 2; ++pending) fixture_between_siblings(notified, pending);
  fixture_preflight_and_epoch(); fixture_self_cycle(); fixture_cycle(); fixture_failure_rearm(); fixture_real_timer();
  fixture_taking(0); fixture_taking(1); fixture_full_resources(); fixture_publishing_parent();
  fixture_shutdown_tightens(); fixture_clock_fault(0); fixture_clock_fault(1); fixture_damaged_pair();
  puts("task-cleanup-wait-v1-ok"); return 0;
}
"#;
