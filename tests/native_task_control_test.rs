//! Internal task control arbitration over real, serially executed R1 frames.
//! This does not enable source async lowering or install a native scheduler.

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

fn sync_program(source: &str) -> ir::IrProgram {
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    ir::lower_program(&ast).unwrap()
}

fn fixture_source() -> String {
    let sync = sync_program("fn main() {}");
    let result = IrType::Result(Box::new(IrType::Str));
    let frames = TaskProgram {
        functions: vec![TaskFunction {
            id: TaskFunctionId(0),
            name: "ControlOwnedFrame".into(),
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
    for name in [
        "ku_task_control_init",
        "KuTaskControlV1",
        "KU_TASK_CONTROL_PENDING",
    ] {
        let collision = sync_program(&format!("fn {name}() {{}} fn main() {{}}"));
        assert!(c::generate_task_frame_c_source(&collision, &frames)
            .unwrap_err()
            .message
            .contains("collides"));
    }
    c::generate_task_frame_c_source(&sync, &frames).unwrap()
}

#[test]
fn native_task_control_frame_arbitration_and_references_execute_in_c() {
    let generated = fixture_source();
    for forbidden in ["run_source", "const SOURCE", "task.spawn", "Task.new"] {
        assert!(!generated.contains(forbidden));
    }
    assert!(generated.contains("KuTaskControlV1"));
    let mut source = generated
        .replacen(
            "typedef struct KuString {",
            &format!("{ALLOCATION_HOOK}\ntypedef struct KuString {{"),
            1,
        )
        .replacen(
            "int main(void) {",
            "static int ku_generated_main(void) {",
            1,
        );
    source.push_str(NATIVE_THREAD_LIFECYCLE_HARNESS);
    source.push_str(C_MAIN);
    let directory = TempDir::new("task-control-v1");
    let c_file = directory.path().join("control.c");
    fs::write(&c_file, source).expect("write generated task control harness");
    let Some(executable) = compile_harness(directory.path(), &c_file, "control") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute the native task control fixture"
        );
        return;
    };
    fs::remove_file(&c_file).expect("remove C source before executing artifact");
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("task control fixture must be bounded");
    assert!(
        output.status.success(),
        "task control lifecycle failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-control-v1-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const C_MAIN: &str = r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr, "control check line %d\n", __LINE__); abort(); } } while (0)
#define CONTROL_ABI KU_TASK_CONTROL_ABI_VERSION
#define FRAME_ABI KU_TASK_FRAME_ABI_VERSION
typedef struct FixtureContext {
  KuTaskControlV1 control;
  KuTaskFrame_0* frame;
  KuResult_str payload;
  bool payload_initialized;
  uint32_t outcome;
  bool pause_ready;
  bool pause_take;
  bool pause_cleanup;
  bool defer_cleanup;
  uint64_t now;
  uint64_t observed_deadline;
  unsigned resume_calls;
  unsigned cleanup_calls;
  unsigned frame_drops;
  unsigned payload_drops;
  unsigned takes;
  unsigned disposes;
  unsigned* heap_disposes;
  KuTestEvent entered;
  KuTestEvent proceed;
} FixtureContext;

/* The allocation ledger is deliberately not accessed concurrently: main owns
 * fixture allocation, executor owns callbacks, and main waits for OS join before
 * reading counters or disposing any payload. The control itself uses atomics. */
static uint64_t fixture_now(void* raw) { return ((FixtureContext*)raw)->now; }
static KuString fixture_owned(const char* text) {
  size_t length = strlen(text);
  uint8_t* data = (uint8_t*)malloc(length ? length : 1);
  CHECK(data);
  if (length) memcpy(data, text, length);
  return (KuString){ data, length, length ? length : 1, KU_STRING_OWNED };
}
static uint32_t fixture_resume(void* raw) {
  FixtureContext* context = (FixtureContext*)raw;
  ++context->resume_calls;
  KuTaskFrameClockV1 clock = { fixture_now, context };
  uint32_t polled = ku_task_frame_0_resume(context->frame, sizeof(*context->frame), FRAME_ABI, &clock);
  if (polled == KU_TASK_FRAME_PENDING) return KU_TASK_CONTROL_PENDING;
  CHECK(polled == KU_TASK_FRAME_READY && !context->payload_initialized);
  CHECK(ku_task_frame_0_take_result(context->frame, sizeof(*context->frame), FRAME_ABI, &context->payload) == KU_TASK_FRAME_OK);
  context->payload_initialized = true;
  if (context->pause_ready) {
    CHECK(ku_test_event_set(&context->entered));
    CHECK(ku_test_event_wait(&context->proceed, 2000));
  }
  /* PANICKED exercises the internal terminal tag with an owned Error payload;
   * it is not a claim of source panic/async lowering. */
  return context->outcome;
}
static uint32_t fixture_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {
  FixtureContext* context = (FixtureContext*)raw;
  ++context->cleanup_calls;
  context->observed_deadline = ku_task_control_cleanup_deadline(budget);
  if (context->pause_cleanup) {
    CHECK(ku_test_event_set(&context->entered));
    CHECK(ku_test_event_wait(&context->proceed, 2000));
    /* A compiler adapter must reread the live budget at cleanup safepoints. */
    context->observed_deadline = ku_task_control_cleanup_deadline(budget);
  }
  if (context->defer_cleanup && context->cleanup_calls == 1) return KU_TASK_CONTROL_PENDING;
  KuTaskFrameClockV1 clock = { fixture_now, context };
  uint32_t frame_reason = reason == KU_TASK_CONTROL_TIMED_OUT ? KU_TASK_FRAME_TIMED_OUT : KU_TASK_FRAME_CANCELLED;
  CHECK(ku_task_frame_0_terminate(context->frame, sizeof(*context->frame), FRAME_ABI,
      frame_reason, context->observed_deadline, &clock) == frame_reason);
  return KU_TASK_CONTROL_OK;
}
static void fixture_drop_frame(void* raw) {
  FixtureContext* context = (FixtureContext*)raw;
  CHECK(context->frame && context->frame_drops == 0);
  CHECK(ku_task_frame_0_destroy(context->frame, sizeof(*context->frame), FRAME_ABI) == KU_TASK_FRAME_OK);
  free(context->frame);
  context->frame = NULL;
  ++context->frame_drops;
}
static void fixture_drop_payload(void* raw) {
  FixtureContext* context = (FixtureContext*)raw;
  CHECK(context->payload_initialized && context->payload_drops == 0 && context->takes == 0);
  ku_result_drop_str(&context->payload);
  context->payload_initialized = false;
  ++context->payload_drops;
}
static uint32_t fixture_take_payload(void* raw, void* output) {
  FixtureContext* context = (FixtureContext*)raw;
  KuResult_str* target = (KuResult_str*)output;
  if (!ku_task_frame_storage_valid(target, sizeof(*target), sizeof(*target), KU_TASK_FRAME_ALIGNOF(KuResult_str))) return KU_TASK_CONTROL_INVALID_ARGUMENT;
  if (target->ok || target->value.ptr || target->value.len || target->value.capacity || target->value.storage
      || target->error.domain.ptr || target->error.code.ptr || target->error.message.ptr) return KU_TASK_CONTROL_INVALID_ARGUMENT;
  CHECK(context->payload_initialized && context->takes == 0);
  if (context->pause_take) {
    CHECK(ku_test_event_set(&context->entered));
    CHECK(ku_test_event_wait(&context->proceed, 2000));
  }
  *target = ku_result_move_str(&context->payload);
  context->payload_initialized = false;
  ++context->takes;
  return KU_TASK_CONTROL_OK;
}
static void fixture_dispose(KuTaskControlV1* control, void* raw) {
  FixtureContext* context = (FixtureContext*)raw;
  CHECK(control == &context->control && context->disposes == 0);
  CHECK(!context->frame && !context->payload_initialized && context->frame_drops == 1);
  ++context->disposes;
}
static const KuTaskControlOpsV1 fixture_ops = {
  fixture_resume, fixture_cleanup, fixture_drop_frame,
  fixture_drop_payload, fixture_take_payload, fixture_dispose
};
static void fixture_init_with_ops(FixtureContext* context, KuTaskControlOwnerV1* owner,
                                 uint32_t outcome, const KuTaskControlOpsV1* operations) {
  memset(context, 0, sizeof(*context));
  memset(owner, 0, sizeof(*owner));
  context->outcome = outcome;
  context->now = 100;
  CHECK(ku_test_event_init(&context->entered));
  CHECK(ku_test_event_init(&context->proceed));
  context->frame = (KuTaskFrame_0*)calloc(1, sizeof(*context->frame));
  CHECK(context->frame);
  KuResult_str input = {0};
  input.ok = outcome == KU_TASK_CONTROL_COMPLETED;
  if (input.ok) input.value = fixture_owned("owned-result");
  else {
    input.error.domain = fixture_owned("task");
    input.error.code = fixture_owned(outcome == KU_TASK_CONTROL_PANICKED ? "panic" : "error");
    input.error.message = fixture_owned("owned-error");
  }
  KuString cleanup = fixture_owned("cleanup");
  CHECK(ku_task_frame_0_init(context->frame, sizeof(*context->frame), FRAME_ABI, &input, &cleanup) == KU_TASK_FRAME_OK);
  CHECK(ku_task_control_init(&context->control, sizeof(context->control), 0, operations, context, owner) == KU_TASK_CONTROL_ABI_MISMATCH);
  CHECK(ku_task_control_init(&context->control, sizeof(context->control) - 1, CONTROL_ABI, operations, context, owner) == KU_TASK_CONTROL_INVALID_ARGUMENT);
  KuTaskControlOpsV1 incomplete = *operations;
  incomplete.resume = NULL;
  CHECK(ku_task_control_init(&context->control, sizeof(context->control), CONTROL_ABI, &incomplete, context, owner) == KU_TASK_CONTROL_INVALID_ARGUMENT);
  CHECK(!owner->lease.control && ku_task_frame_zero_bytes(&context->control, sizeof(context->control)));
  CHECK(ku_task_control_init(&context->control, sizeof(context->control), CONTROL_ABI, operations, context, owner) == KU_TASK_CONTROL_OK);
}
static void fixture_init(FixtureContext* context, KuTaskControlOwnerV1* owner, uint32_t outcome) {
  fixture_init_with_ops(context, owner, outcome, &fixture_ops);
}
static void fixture_finish(FixtureContext* context) {
  CHECK(context->disposes == 1 && context->frame_drops == 1);
  CHECK(!context->frame && !context->payload_initialized);
  CHECK(ku_test_event_destroy(&context->entered));
  CHECK(ku_test_event_destroy(&context->proceed));
  CHECK(ku_perf_live_allocations == 0 && ku_perf_live_bytes == 0 && !ku_perf_overflow);
}
typedef struct FixtureWorker {
  KuTaskControlLeaseV1 lease;
  uint32_t observed;
} FixtureWorker;
static int fixture_worker_poll(void* raw) {
  FixtureWorker* worker = (FixtureWorker*)raw;
  worker->observed = ku_task_control_poll(&worker->lease);
  return 0;
}
static void fixture_cancel_beats_private_result(uint32_t outcome, uint32_t reason, bool owner_drop) {
  FixtureContext context;
  KuTaskControlOwnerV1 owner;
  fixture_init(&context, &owner, outcome);
  KuTaskControlLeaseV1 observer = {0};
  FixtureWorker worker = {0};
  CHECK(ku_task_control_lease_retain(&owner.lease, &observer) == KU_TASK_CONTROL_OK);
  CHECK(ku_task_control_lease_retain(&owner.lease, &worker.lease) == KU_TASK_CONTROL_OK);
  CHECK(ku_task_control_poll(&observer) == KU_TASK_CONTROL_PENDING);
  context.pause_ready = true;
  KuTestThread thread;
  CHECK(ku_test_thread_start(&thread, fixture_worker_poll, &worker));
  CHECK(ku_test_event_wait(&context.entered, 2000));
  /* Only control APIs run here: never concurrently touch the serial R1 frame. */
  CHECK(ku_task_control_poll(&observer) == KU_TASK_CONTROL_PENDING);
  if (owner_drop) {
    CHECK(reason == KU_TASK_CONTROL_CANCELLED);
    CHECK(ku_task_control_owner_drop(&owner, 1000) == KU_TASK_CONTROL_OK);
  } else {
    CHECK(ku_task_control_request_cancel(&observer, reason, 1000) == KU_TASK_CONTROL_OK);
  }
  CHECK(ku_task_control_status(&observer) == KU_TASK_CONTROL_PENDING);
  CHECK(ku_test_event_set(&context.proceed));
  CHECK(ku_test_thread_join(&thread, 2000) && thread.outcome == 0);
  CHECK(worker.observed == reason && ku_task_control_status(&observer) == reason);
  CHECK(context.frame_drops == 1 && context.payload_drops == 1 && context.takes == 0);
  CHECK(context.cleanup_calls == 0 && context.resume_calls == 2);
  CHECK(ku_perf_live_allocations == 0 && ku_perf_live_bytes == 0);
  KuResult_str output = {0};
  CHECK(ku_task_control_take_result(&observer, &output) != KU_TASK_CONTROL_OK);
  CHECK(!output.ok && !output.value.ptr && !output.error.message.ptr);
  if (!owner_drop) CHECK(ku_task_control_owner_drop(&owner, 2000) == KU_TASK_CONTROL_OK);
  CHECK(context.disposes == 0);
  CHECK(ku_task_control_lease_release(&worker.lease) == KU_TASK_CONTROL_OK);
  CHECK(context.disposes == 0);
  CHECK(ku_task_control_lease_release(&observer) == KU_TASK_CONTROL_OK);
  fixture_finish(&context);
}
static void fixture_completion_wins(uint32_t outcome, bool take) {
  FixtureContext context;
  KuTaskControlOwnerV1 owner;
  fixture_init(&context, &owner, outcome);
  KuTaskControlOwnerV1 moved = {0};
  CHECK(ku_task_control_owner_move(&moved, &owner) == KU_TASK_CONTROL_OK);
  CHECK(!owner.lease.control);
  KuTaskControlLeaseV1 observer = {0};
  CHECK(ku_task_control_lease_retain(&moved.lease, &observer) == KU_TASK_CONTROL_OK);
  CHECK(ku_task_control_poll(&observer) == KU_TASK_CONTROL_PENDING);
  CHECK(ku_task_control_poll(&observer) == outcome);
  CHECK(context.frame_drops == 1 && context.payload_initialized);
  CHECK(ku_task_control_request_cancel(&observer, KU_TASK_CONTROL_CANCELLED, 1000) == outcome);
  CHECK(ku_task_control_status(&observer) == outcome);
  CHECK(ku_task_control_poll(&observer) == outcome && context.resume_calls == 2);
  if (take) {
    KuResult_str occupied = {0}; occupied.ok = true; occupied.value = fixture_owned("occupied");
    CHECK(ku_task_control_take_result(&observer, &occupied) == KU_TASK_CONTROL_INVALID_ARGUMENT);
    CHECK(occupied.value.len == 8 && context.payload_initialized && !context.takes);
    ku_result_drop_str(&occupied);
    KuResult_str output = {0};
    CHECK(ku_task_control_take_result(&observer, &output) == KU_TASK_CONTROL_OK);
    CHECK(output.ok == (outcome == KU_TASK_CONTROL_COMPLETED));
    if (output.ok) CHECK(output.value.len == 12);
    else CHECK(output.error.domain.len == 4 && output.error.message.len == 11);
    CHECK(ku_task_control_take_result(&observer, &output) == KU_TASK_CONTROL_RESULT_TAKEN);
    ku_result_drop_str(&output);
  }
  CHECK(ku_task_control_owner_drop(&moved, 500) == KU_TASK_CONTROL_OK);
  CHECK(context.disposes == 0);
  if (!take) {
    CHECK(context.payload_drops == 1 && !context.payload_initialized);
    KuResult_str empty = {0};
    CHECK(ku_task_control_take_result(&observer, &empty) == KU_TASK_CONTROL_RESULT_TAKEN);
  }
  CHECK(ku_task_control_lease_release(&observer) == KU_TASK_CONTROL_OK);
  CHECK(context.payload_drops == (take ? 0u : 1u) && context.takes == (take ? 1u : 0u));
  fixture_finish(&context);
}
static void fixture_cleanup_pin_and_shared_deadline(uint32_t reason) {
  FixtureContext context;
  KuTaskControlOwnerV1 owner;
  fixture_init(&context, &owner, KU_TASK_CONTROL_FAILED);
  KuTaskControlLeaseV1 worker = {0};
  CHECK(ku_task_control_lease_retain(&owner.lease, &worker) == KU_TASK_CONTROL_OK);
  CHECK(ku_task_control_poll(&worker) == KU_TASK_CONTROL_PENDING);
  context.defer_cleanup = true;
  CHECK(ku_task_control_request_cancel(&worker, reason, 1000) == KU_TASK_CONTROL_OK);
  CHECK(ku_task_control_poll(&worker) == KU_TASK_CONTROL_PENDING);
  CHECK(context.cleanup_calls == 1 && context.observed_deadline == 1000 && context.frame_drops == 0);
  CHECK(ku_task_control_atomic_load(&context.control.lifecycle_pin) == 1);
  CHECK(ku_task_control_owner_drop(&owner, 800) == KU_TASK_CONTROL_OK);
  CHECK(context.disposes == 0 && !owner.lease.control);
  CHECK(ku_task_control_atomic_load(&context.control.references) == 2);
  CHECK(ku_task_control_cleanup_deadline(&context.control) == 800);
  uint32_t other_reason = reason == KU_TASK_CONTROL_CANCELLED ? KU_TASK_CONTROL_TIMED_OUT : KU_TASK_CONTROL_CANCELLED;
  CHECK(ku_task_control_request_cancel(&worker, other_reason, 500) == KU_TASK_CONTROL_OK);
  CHECK(ku_task_control_request_cancel(&worker, other_reason, 2000) == KU_TASK_CONTROL_OK);
  CHECK(ku_task_control_cleanup_deadline(&context.control) == 500);
  context.now = 600;
  CHECK(ku_task_control_poll(&worker) == reason);
  CHECK(context.cleanup_calls == 2 && context.observed_deadline == 500);
  CHECK(context.frame_drops == 1 && context.payload_drops == 0 && context.resume_calls == 1);
  CHECK(context.disposes == 0 && ku_perf_live_allocations == 0 && ku_perf_live_bytes == 0);
  CHECK(ku_task_control_atomic_load(&context.control.lifecycle_pin) == 0);
  CHECK(ku_task_control_atomic_load(&context.control.references) == 1);
  CHECK(ku_task_control_lease_release(&worker) == KU_TASK_CONTROL_OK);
  fixture_finish(&context);
}
typedef struct FixtureTakeWorker {
  KuTaskControlLeaseV1 lease;
  KuResult_str output;
  uint32_t observed;
} FixtureTakeWorker;
static int fixture_worker_take(void* raw) {
  FixtureTakeWorker* worker = (FixtureTakeWorker*)raw;
  worker->observed = ku_task_control_take_result(&worker->lease, &worker->output);
  return 0;
}
static void fixture_take_blocks_owner_drop(uint32_t outcome) {
  FixtureContext context;
  KuTaskControlOwnerV1 owner;
  fixture_init(&context, &owner, outcome);
  KuTaskControlLeaseV1 observer = {0};
  FixtureTakeWorker worker = {0};
  CHECK(ku_task_control_lease_retain(&owner.lease, &observer) == KU_TASK_CONTROL_OK);
  CHECK(ku_task_control_lease_retain(&owner.lease, &worker.lease) == KU_TASK_CONTROL_OK);
  CHECK(ku_task_control_poll(&observer) == KU_TASK_CONTROL_PENDING);
  CHECK(ku_task_control_poll(&observer) == outcome);
  context.pause_take = true;
  KuTestThread thread;
  CHECK(ku_test_thread_start(&thread, fixture_worker_take, &worker));
  CHECK(ku_test_event_wait(&context.entered, 2000));
  KuResult_str untouched = {0};
  CHECK(ku_task_control_take_result(&observer, &untouched) == KU_TASK_CONTROL_PENDING);
  CHECK(!untouched.ok && !untouched.value.ptr && !untouched.error.message.ptr);
  CHECK(ku_task_control_owner_drop(&owner, 1000) == KU_TASK_CONTROL_PENDING);
  CHECK(owner.lease.control == &context.control);
  CHECK(ku_test_event_set(&context.proceed));
  CHECK(ku_test_thread_join(&thread, 2000) && thread.outcome == 0);
  CHECK(worker.observed == KU_TASK_CONTROL_OK && context.takes == 1 && context.payload_drops == 0);
  CHECK(worker.output.ok == (outcome == KU_TASK_CONTROL_COMPLETED));
  CHECK(ku_task_control_take_result(&observer, &untouched) == KU_TASK_CONTROL_RESULT_TAKEN);
  ku_result_drop_str(&worker.output);
  CHECK(ku_task_control_owner_drop(&owner, 1000) == KU_TASK_CONTROL_OK);
  CHECK(context.disposes == 0 && !owner.lease.control);
  CHECK(ku_task_control_lease_release(&worker.lease) == KU_TASK_CONTROL_OK);
  CHECK(context.disposes == 0);
  CHECK(ku_task_control_lease_release(&observer) == KU_TASK_CONTROL_OK);
  fixture_finish(&context);
}
static void fixture_live_cleanup_budget(uint32_t reason) {
  FixtureContext context;
  KuTaskControlOwnerV1 owner;
  fixture_init(&context, &owner, KU_TASK_CONTROL_FAILED);
  FixtureWorker worker = {0};
  KuTaskControlLeaseV1 observer = {0};
  CHECK(ku_task_control_lease_retain(&owner.lease, &worker.lease) == KU_TASK_CONTROL_OK);
  CHECK(ku_task_control_lease_retain(&owner.lease, &observer) == KU_TASK_CONTROL_OK);
  CHECK(ku_task_control_poll(&observer) == KU_TASK_CONTROL_PENDING);
  context.pause_cleanup = true;
  context.now = 600;
  CHECK(ku_task_control_request_cancel(&observer, reason, 1000) == KU_TASK_CONTROL_OK);
  KuTestThread thread;
  CHECK(ku_test_thread_start(&thread, fixture_worker_poll, &worker));
  CHECK(ku_test_event_wait(&context.entered, 2000));
  CHECK(ku_task_control_cleanup_deadline(&context.control) == 1000);
  uint32_t other = reason == KU_TASK_CONTROL_CANCELLED ? KU_TASK_CONTROL_TIMED_OUT : KU_TASK_CONTROL_CANCELLED;
  CHECK(ku_task_control_request_cancel(&observer, other, 500) == KU_TASK_CONTROL_OK);
  CHECK(ku_task_control_owner_drop(&owner, 800) == KU_TASK_CONTROL_OK);
  CHECK(ku_task_control_cleanup_deadline(&context.control) == 500);
  CHECK(ku_test_event_set(&context.proceed));
  CHECK(ku_test_thread_join(&thread, 2000) && thread.outcome == 0);
  CHECK(worker.observed == reason && context.observed_deadline == 500);
  CHECK(context.cleanup_calls == 1 && context.frame_drops == 1 && context.payload_drops == 0);
  CHECK(context.disposes == 0 && ku_perf_live_allocations == 0 && ku_perf_live_bytes == 0);
  CHECK(ku_task_control_lease_release(&worker.lease) == KU_TASK_CONTROL_OK);
  CHECK(ku_task_control_lease_release(&observer) == KU_TASK_CONTROL_OK);
  fixture_finish(&context);
}
static void fixture_reference_and_init_boundaries(void) {
  FixtureContext context;
  KuTaskControlOwnerV1 owner;
  fixture_init(&context, &owner, KU_TASK_CONTROL_COMPLETED);
  size_t count = KU_TASK_CONTROL_MAX_REFERENCES - 2;
  KuTaskControlLeaseV1* leases = (KuTaskControlLeaseV1*)calloc(count, sizeof(*leases));
  CHECK(leases);
  for (size_t index = 0; index < count; ++index)
    CHECK(ku_task_control_lease_retain(&owner.lease, &leases[index]) == KU_TASK_CONTROL_OK);
  CHECK(ku_task_control_atomic_load(&context.control.references) == KU_TASK_CONTROL_MAX_REFERENCES);
  KuTaskControlLeaseV1 overflow = {0};
  CHECK(ku_task_control_lease_retain(&owner.lease, &overflow) == KU_TASK_CONTROL_LIMIT && !overflow.control);
  CHECK(ku_task_control_lease_retain(&owner.lease, &leases[0]) == KU_TASK_CONTROL_INVALID_STATE);
  CHECK(ku_task_control_lease_retain(&owner.lease, &owner.lease) == KU_TASK_CONTROL_INVALID_ARGUMENT);
  KuTaskControlOwnerV1 empty = {0};
  CHECK(ku_task_control_owner_move(&owner, &empty) == KU_TASK_CONTROL_INVALID_ARGUMENT);
  CHECK(owner.lease.control == &context.control);
  CHECK(ku_task_control_owner_move(&owner, &owner) == KU_TASK_CONTROL_INVALID_ARGUMENT);
  CHECK(ku_task_control_init(&context.control, sizeof(context.control), CONTROL_ABI, &fixture_ops, &context, &owner) == KU_TASK_CONTROL_INVALID_STATE);
  for (size_t index = 1; index < count; ++index)
    CHECK(ku_task_control_lease_release(&leases[index]) == KU_TASK_CONTROL_OK);
  CHECK(ku_task_control_lease_release(&leases[1]) == KU_TASK_CONTROL_INVALID_ARGUMENT);
  CHECK(ku_task_control_atomic_load(&context.control.references) == 3);
  CHECK(ku_task_control_owner_drop(&owner, 1000) == KU_TASK_CONTROL_OK);
  CHECK(ku_task_control_poll(&leases[0]) == KU_TASK_CONTROL_CANCELLED);
  CHECK(context.resume_calls == 0 && context.cleanup_calls == 1);
  CHECK(ku_task_control_lease_release(&leases[0]) == KU_TASK_CONTROL_OK);
  free(leases);
  fixture_finish(&context);
}
static void fixture_heap_dispose(KuTaskControlV1* control, void* raw) {
  FixtureContext* context = (FixtureContext*)raw;
  CHECK(control == &context->control && context->disposes == 0);
  CHECK(!context->frame && !context->payload_initialized && context->frame_drops == 1);
  CHECK(context->takes == 0);
  CHECK((context->resume_calls == 2 && context->payload_drops == 1 && context->cleanup_calls == 0)
      || (context->resume_calls == 1 && context->payload_drops == 0 && context->cleanup_calls == 1));
  CHECK(context->heap_disposes && *context->heap_disposes == 0);
  CHECK(ku_test_event_destroy(&context->entered));
  CHECK(ku_test_event_destroy(&context->proceed));
  ++*context->heap_disposes;
  /* Unlike the stack fixtures this actually invalidates control and context.
   * ASan must catch any access after dispose returns to unref/release/drop. */
  free(context);
}
static const KuTaskControlOpsV1 fixture_heap_ops = {
  fixture_resume, fixture_cleanup, fixture_drop_frame,
  fixture_drop_payload, fixture_take_payload, fixture_heap_dispose
};
typedef struct FixtureHeapOwnerWorker {
  KuTaskControlOwnerV1 owner;
  uint32_t observed;
} FixtureHeapOwnerWorker;
static int fixture_heap_owner_worker(void* raw) {
  FixtureHeapOwnerWorker* worker = (FixtureHeapOwnerWorker*)raw;
  worker->observed = ku_task_control_owner_drop(&worker->owner, 1000);
  return 0;
}
static void fixture_heap_last_owner_drop(uint32_t outcome) {
  unsigned disposed = 0;
  FixtureContext* context = (FixtureContext*)calloc(1, sizeof(*context));
  CHECK(context);
  KuTaskControlOwnerV1 owner;
  fixture_init_with_ops(context, &owner, outcome, &fixture_heap_ops);
  context->heap_disposes = &disposed;
  CHECK(ku_task_control_poll(&owner.lease) == KU_TASK_CONTROL_PENDING);
  CHECK(ku_task_control_poll(&owner.lease) == outcome);
  CHECK(context->payload_initialized && context->frame_drops == 1);
  CHECK(ku_task_control_atomic_load(&context->control.references) == 1);
  FixtureHeapOwnerWorker worker = {0};
  CHECK(ku_task_control_owner_move(&worker.owner, &owner) == KU_TASK_CONTROL_OK);
  KuTestThread thread;
  CHECK(ku_test_thread_start(&thread, fixture_heap_owner_worker, &worker));
  /* No context/control access from this point: the worker can free both. */
  CHECK(ku_test_thread_join(&thread, 2000) && thread.outcome == 0);
  CHECK(worker.observed == KU_TASK_CONTROL_OK && !worker.owner.lease.control);
  CHECK(disposed == 1);
  CHECK(ku_perf_live_allocations == 0 && ku_perf_live_bytes == 0 && !ku_perf_overflow);
}
typedef struct FixtureHeapLeaseWorker {
  KuTaskControlLeaseV1 lease;
  uint32_t polled;
  uint32_t released;
} FixtureHeapLeaseWorker;
static int fixture_heap_lease_worker(void* raw) {
  FixtureHeapLeaseWorker* worker = (FixtureHeapLeaseWorker*)raw;
  worker->polled = ku_task_control_poll(&worker->lease);
  worker->released = ku_task_control_lease_release(&worker->lease);
  return 0;
}
static void fixture_heap_last_worker_release(void) {
  unsigned disposed = 0;
  FixtureContext* context = (FixtureContext*)calloc(1, sizeof(*context));
  CHECK(context);
  KuTaskControlOwnerV1 owner;
  fixture_init_with_ops(context, &owner, KU_TASK_CONTROL_FAILED, &fixture_heap_ops);
  context->heap_disposes = &disposed;
  FixtureHeapLeaseWorker worker = {0};
  CHECK(ku_task_control_lease_retain(&owner.lease, &worker.lease) == KU_TASK_CONTROL_OK);
  CHECK(ku_task_control_poll(&worker.lease) == KU_TASK_CONTROL_PENDING);
  CHECK(ku_task_control_owner_drop(&owner, 1000) == KU_TASK_CONTROL_OK);
  CHECK(!owner.lease.control && context->frame_drops == 0);
  CHECK(ku_task_control_atomic_load(&context->control.references) == 2);
  KuTestThread thread;
  CHECK(ku_test_thread_start(&thread, fixture_heap_lease_worker, &worker));
  /* The executor alone terminates/destroys the R1 frame, releases the pin,
   * then drops its last lease and really frees the heap control/context. */
  CHECK(ku_test_thread_join(&thread, 2000) && thread.outcome == 0);
  CHECK(worker.polled == KU_TASK_CONTROL_CANCELLED);
  CHECK(worker.released == KU_TASK_CONTROL_OK && !worker.lease.control);
  CHECK(disposed == 1);
  CHECK(ku_perf_live_allocations == 0 && ku_perf_live_bytes == 0 && !ku_perf_overflow);
}
int main(void) {
  const uint32_t outcomes[] = { KU_TASK_CONTROL_COMPLETED, KU_TASK_CONTROL_FAILED, KU_TASK_CONTROL_PANICKED };
  for (unsigned round = 0; round < 64; ++round) {
    for (unsigned index = 0; index < 3; ++index) {
      fixture_cancel_beats_private_result(outcomes[index], KU_TASK_CONTROL_CANCELLED, false);
      fixture_cancel_beats_private_result(outcomes[index], KU_TASK_CONTROL_TIMED_OUT, false);
      fixture_cancel_beats_private_result(outcomes[index], KU_TASK_CONTROL_CANCELLED, true);
      fixture_completion_wins(outcomes[index], false);
      fixture_completion_wins(outcomes[index], true);
      fixture_take_blocks_owner_drop(outcomes[index]);
    }
    fixture_cleanup_pin_and_shared_deadline(KU_TASK_CONTROL_CANCELLED);
    fixture_cleanup_pin_and_shared_deadline(KU_TASK_CONTROL_TIMED_OUT);
    fixture_live_cleanup_budget(KU_TASK_CONTROL_CANCELLED);
    fixture_live_cleanup_budget(KU_TASK_CONTROL_TIMED_OUT);
    fixture_heap_last_owner_drop(KU_TASK_CONTROL_COMPLETED);
    fixture_heap_last_owner_drop(KU_TASK_CONTROL_FAILED);
    fixture_heap_last_worker_release();
  }
  fixture_reference_and_init_boundaries();
  CHECK(ku_perf_live_allocations == 0 && ku_perf_live_bytes == 0 && !ku_perf_overflow);
  puts("task-control-v1-ok");
  return 0;
}
"#;
