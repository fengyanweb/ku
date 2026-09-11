//! Fixed-work native Driver7 observation tool, not a performance acceptance test.
//! Generate once, compile the same C with KU_BENCH_OBSERVE=0 and =1 at identical
//! optimization/link settings, then run each binary through the bounded matrix.
#[allow(dead_code)]
#[path = "../tests/support/bounded_process.rs"]
mod bounded_process;
#[path = "../tests/support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[path = "../tests/support/native_task_ledger.rs"]
mod native_task_ledger;
#[cfg(test)]
#[allow(dead_code)]
#[path = "../tests/support/native_pg_harness.rs"]
mod probe_native_harness;

use ku::{
    backend::c,
    checker::Checker,
    ir::{self, task::*, IrType},
    lexer::Lexer,
    parser::Parser,
};
use std::{error::Error, fs, path::PathBuf, process::Command, time::Duration};

fn tasks() -> TaskProgram {
    let value = |ty| TaskSlot {
        ty: TaskSlotType::Value {
            ty,
            borrowed: false,
        },
    };
    TaskProgram {
        functions: vec![TaskFunction {
            id: TaskFunctionId(0),
            name: "FixedWorkDriverProbe".into(),
            slots: vec![
                value(IrType::Int),
                value(IrType::Str),
                value(IrType::Int),
                value(IrType::Int),
                value(IrType::Int),
                value(IrType::Bool),
                value(IrType::Result(Box::new(IrType::Int))),
            ],
            parameters: vec![SlotId(0), SlotId(1)],
            entry: StateId(0),
            result: IrType::Result(Box::new(IrType::Int)),
            states: vec![
                TaskState {
                    operations: vec![
                        TaskOp::Init {
                            dst: SlotId(2),
                            value: TaskConstant::Int(1),
                        },
                        TaskOp::Init {
                            dst: SlotId(3),
                            value: TaskConstant::Int(0),
                        },
                    ],
                    terminator: TaskTerminator::Jump { target: StateId(1) },
                },
                TaskState {
                    operations: vec![
                        TaskOp::Binary {
                            dst: SlotId(4),
                            op: TaskBinaryOp::Subtract,
                            left: SlotId(0),
                            right: SlotId(2),
                        },
                        TaskOp::Copy {
                            dst: SlotId(0),
                            src: SlotId(4),
                        },
                        TaskOp::Binary {
                            dst: SlotId(5),
                            op: TaskBinaryOp::Greater,
                            left: SlotId(0),
                            right: SlotId(3),
                        },
                    ],
                    terminator: TaskTerminator::Branch {
                        condition: SlotId(5),
                        then_state: StateId(2),
                        else_state: StateId(3),
                    },
                },
                TaskState {
                    operations: vec![],
                    terminator: TaskTerminator::Suspend {
                        resume: StateId(1),
                        cleanup: StateId(4),
                    },
                },
                TaskState {
                    operations: vec![
                        TaskOp::Drop { slot: SlotId(1) },
                        TaskOp::WrapOk {
                            dst: SlotId(6),
                            src: SlotId(0),
                        },
                    ],
                    terminator: TaskTerminator::Complete { value: SlotId(6) },
                },
                TaskState {
                    operations: vec![TaskOp::Drop { slot: SlotId(1) }],
                    terminator: TaskTerminator::Terminate,
                },
            ],
        }],
    }
}

fn emit(path: PathBuf) -> Result<(), Box<dyn Error>> {
    let ast = Parser::new(Lexer::new("fn main() {}").lex()?).parse_program()?;
    Checker::new().check(&ast)?;
    let program = tasks();
    verify_and_plan(&program, TaskLimits::default())?;
    let generated = c::generate_task_frame_c_source(&ir::lower_program(&ast)?, &program)?;
    let ledger = format!(
        "#if defined(_WIN32)\n#ifndef WIN32_LEAN_AND_MEAN\n#define WIN32_LEAN_AND_MEAN\n#endif\n#include <windows.h>\n#else\n#include <pthread.h>\n#endif\n{}\n{}\n{}\n",
        native_task_ledger::LEDGER_LOCK,
        native_allocation_harness::ALLOCATION_HOOK,
        native_task_ledger::LOCKED_ALLOCATIONS,
    );
    let clock = "static uint64_t ku_task_driver_now_ms(void) {";
    let returned =
        "slot = &driver->slots[index];\n    if (driver->polls != UINT64_MAX) driver->polls++;";
    for unique in [
        "typedef struct KuString {",
        "int main(void) {",
        clock,
        returned,
    ] {
        assert_eq!(generated.matches(unique).count(), 1, "{unique}");
    }
    let mut source = generated
        .replacen("typedef struct KuString {", &format!("{ledger}typedef struct KuString {{"), 1)
        .replacen("int main(void) {", "static int ku_generated_main(void) {", 1)
        .replacen(
            clock,
            &format!(
                "#ifndef KU_BENCH_OBSERVE\n#define KU_BENCH_OBSERVE 0\n#endif\n\
                 #if KU_BENCH_OBSERVE != 0 && KU_BENCH_OBSERVE != 1\n#error invalid_observer_mode\n#endif\n\
                 static void bench_returned_locked(const KuTaskDriverWorkerV1*, size_t);\n{clock}"
            ),
            1,
        )
        .replacen(
            returned,
            &format!("{returned}\n#if KU_BENCH_OBSERVE\n    bench_returned_locked(worker, index);\n#endif"),
            1,
        );
    source.push_str(HARNESS);
    fs::write(path, source)?;
    Ok(())
}

fn number(value: &str, maximum: usize) -> Result<usize, Box<dyn Error>> {
    let parsed = value.parse::<usize>()?;
    if parsed == 0 || parsed > maximum {
        return Err(format!("expected 1..={maximum}, got {value}").into());
    }
    Ok(parsed)
}

fn validate_row(
    stdout: &[u8],
    workers: usize,
    tasks: usize,
    quanta: usize,
    expected_observer: Option<u64>,
) -> Result<u64, Box<dyn Error>> {
    use ku::value::Value;
    let text = std::str::from_utf8(stdout)?;
    if text.lines().count() != 1 || text.trim().is_empty() {
        return Err("probe must emit exactly one nonempty JSON line".into());
    }
    let parsed = ku::stdlib::json::eval(
        "parse",
        &[Value::String(text.to_owned())],
        ku::span::Span::default(),
    )?;
    let Some(Value::Result { ok: true, value }) = parsed else {
        return Err("probe row is not valid JSON".into());
    };
    let Value::Object(fields) = *value else {
        return Err("probe row must be a JSON object".into());
    };
    if fields.len() != 12 {
        return Err("unexpected probe row schema".into());
    }
    let integer = |key: &str| -> Result<u64, Box<dyn Error>> {
        match fields.get(key) {
            Some(Value::Int(value)) if *value >= 0 => Ok(*value as u64),
            _ => Err(format!("probe field {key} must be a nonnegative integer").into()),
        }
    };
    let expected_polls = (tasks as u64)
        .checked_mul(quanta as u64)
        .ok_or("poll count overflow")?;
    if integer("workers")? != workers as u64
        || integer("tasks")? != tasks as u64
        || integer("quanta")? != quanta as u64
        || integer("returned_polls")? != expected_polls
        || !matches!(fields.get("ledger_zero"), Some(Value::Bool(true)))
    {
        return Err("probe row identity, returned polls or ledger mismatch".into());
    }
    let observer = integer("observer")?;
    if observer > 1 || expected_observer.is_some_and(|expected| expected != observer) {
        return Err("probe observer mode is invalid or changed within one binary matrix".into());
    }
    let frame_bytes = integer("frame_bytes")?;
    let instance_bytes = integer("instance_bytes")?;
    if integer("elapsed_ms")? > 10_000
        || frame_bytes == 0
        || instance_bytes <= frame_bytes
        || integer("fixed_bytes")? == 0
    {
        return Err("probe time or storage metadata is invalid".into());
    }
    if observer == 0 {
        if !matches!(fields.get("migrations"), Some(Value::Null))
            || !matches!(fields.get("per_worker_returned_polls"), Some(Value::Null))
        {
            return Err("uninstrumented probe must report null observer counters".into());
        }
    } else {
        let migrations = integer("migrations")?;
        if migrations
            > expected_polls
                .checked_sub(tasks as u64)
                .ok_or("invalid quanta")?
            || (workers == 1 && migrations != 0)
        {
            return Err("probe migration count violates returned-poll bounds".into());
        }
        let Some(Value::Array(per_worker)) = fields.get("per_worker_returned_polls") else {
            return Err("instrumented probe requires per-worker integer counts".into());
        };
        if per_worker.len() != workers {
            return Err("probe per-worker count length mismatch".into());
        }
        let mut sum = 0u64;
        for value in per_worker {
            let Value::Int(value) = value else {
                return Err("probe per-worker count must be an integer".into());
            };
            let count = u64::try_from(*value)?;
            sum = sum
                .checked_add(count)
                .ok_or("probe per-worker sum overflow")?;
        }
        if sum != expected_polls {
            return Err("probe per-worker counts do not conserve returned polls".into());
        }
    }
    Ok(observer)
}

fn run_matrix(
    executable: PathBuf,
    count: usize,
    quanta: usize,
    repeats: usize,
) -> Result<(), Box<dyn Error>> {
    let executable = executable.canonicalize()?;
    let available = std::thread::available_parallelism()?.get().min(32);
    let mut workers: Vec<usize> = [1, 2, 4, available]
        .into_iter()
        .filter(|n| *n <= available && *n <= count)
        .collect();
    workers.sort_unstable();
    workers.dedup();
    eprintln!(
        "probe_metadata os={} arch={} available_bounded={} tasks={} quanta={} repeats={} workers={:?}",
        std::env::consts::OS, std::env::consts::ARCH, available, count, quanta, repeats, workers
    );
    let mut observer = None;
    for worker_count in workers {
        // Each process has a fresh actual worker group. Round0 warms code/cache;
        // retain its raw row but label it, rather than silently selecting samples.
        for round in 0..=repeats {
            eprintln!(
                "probe_phase workers={worker_count} round={round} warmup={}",
                round == 0
            );
            let mut command = Command::new(&executable);
            command.args([
                worker_count.to_string(),
                count.to_string(),
                quanta.to_string(),
            ]);
            let output = bounded_process::run_bounded(
                &mut command,
                Duration::from_secs(20),
                bounded_process::OutputLimits::new(64 * 1024, 128 * 1024),
            )?;
            print!("{}", String::from_utf8_lossy(&output.stdout));
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            if !output.status.success() {
                return Err(format!(
                    "probe failed: workers={worker_count} round={round} status={}",
                    output.status
                )
                .into());
            }
            if !output.stderr.is_empty() {
                return Err("unexpected C probe stderr; retain the raw failure".into());
            }
            observer = Some(validate_row(
                &output.stdout,
                worker_count,
                count,
                quanta,
                observer,
            )?);
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("emit") if args.len() == 3 => emit(PathBuf::from(&args[2])),
        Some("run") if args.len() == 6 => run_matrix(
            PathBuf::from(&args[2]),
            number(&args[3], 128)?,
            number(&args[4], 65_536)?,
            number(&args[5], 7)?,
        ),
        _ => Err("usage: native_task_scheduler_probe emit OUTPUT.c | run EXECUTABLE TASKS(1..128) QUANTA(1..65536) REPEATS(1..7)".into()),
    }
}

const HARNESS: &str = r#"
enum { BENCH_MAX_TASKS = 128, BENCH_MAX_WORKERS = 32 };
typedef struct BenchTask {
  KuTaskControlV1 control;
  KuTaskFrame_0 frame;
  KuTaskDriverTicketV1 ticket;
  KuResult_int payload;
  int frame_initialized, payload_initialized;
  /* These fields have the SAME layout and admission charge in both builds. */
  uint64_t observed_polls, migrations;
  size_t previous_worker;
} BenchTask;
static BenchTask* bench_tasks[BENCH_MAX_TASKS];
static uint64_t bench_worker_polls[BENCH_MAX_WORKERS];
static size_t bench_count;
static int bench_observing, bench_counter_fault;
/* Called only under the driver's ALREADY HELD post-poll mutex. No additional
 * lock, atomic, clock, OS identity syscall or allocation is added per poll.
 * The extra stores/branch/cache traffic still perturb that critical section. */
static void bench_returned_locked(const KuTaskDriverWorkerV1* worker, size_t index) {
  if (!bench_observing) return;
  CHECK(index < bench_count && worker->index < BENCH_MAX_WORKERS);
  BenchTask* task = bench_tasks[index];
  CHECK(task && task->ticket.slot == index);
  if (task->observed_polls == UINT64_MAX || bench_worker_polls[worker->index] == UINT64_MAX) {
    bench_counter_fault = 1; return;
  }
  if (task->observed_polls && task->previous_worker != worker->index) {
    if (task->migrations == UINT64_MAX) { bench_counter_fault = 1; return; }
    task->migrations++;
  }
  task->previous_worker = worker->index;
  task->observed_polls++;
  bench_worker_polls[worker->index]++;
}
static uint64_t bench_now(void* raw) { (void)raw; return ku_task_driver_now_ms(); }
static uint64_t bench_deadline(uint64_t span) {
  uint64_t now = ku_task_driver_now_ms();
  CHECK(now != UINT64_MAX && span && now < UINT64_MAX - span);
  return now + span;
}
static uint32_t bench_resume(void* raw) {
  BenchTask* task = (BenchTask*)raw;
  KuTaskFrameClockV1 clock = { bench_now, task };
  uint32_t state = ku_task_frame_0_resume(&task->frame, sizeof(task->frame), KU_TASK_FRAME_ABI_VERSION, &clock);
  if (state == KU_TASK_FRAME_PENDING) {
    CHECK(ku_task_driver_set_intent(&task->ticket, KU_TASK_DRIVER_YIELD) == KU_TASK_DRIVER_OK);
    return KU_TASK_CONTROL_PENDING;
  }
  CHECK(state == KU_TASK_FRAME_READY && !task->payload_initialized);
  CHECK(ku_task_frame_0_take_result(&task->frame, sizeof(task->frame), KU_TASK_FRAME_ABI_VERSION, &task->payload) == KU_TASK_FRAME_OK);
  task->payload_initialized = 1;
  return task->payload.ok ? KU_TASK_CONTROL_COMPLETED : KU_TASK_CONTROL_FAILED;
}
static uint32_t bench_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {
  BenchTask* task = (BenchTask*)raw;
  CHECK(task->frame_initialized);
  if (task->frame.header.status == KU_TASK_FRAME_READY) {
    /* Actual frame completion/take may lose R2's reservation to cancellation.
     * R2 has already dropped our private payload. Complete cleared all slot
     * ownership; never replay its stale Suspend cleanup over moved values.
     * R2 still owns terminal publication and calls the real drop_frame next. */
    CHECK(!task->payload_initialized && !task->frame.header.initialized
        && !task->frame.header.result_initialized);
    return KU_TASK_CONTROL_OK;
  }
  KuTaskFrameClockV1 clock = { bench_now, task };
  uint32_t frame_reason = reason == KU_TASK_CONTROL_TIMED_OUT ? KU_TASK_FRAME_TIMED_OUT : KU_TASK_FRAME_CANCELLED;
  CHECK(ku_task_frame_0_terminate(&task->frame, sizeof(task->frame), KU_TASK_FRAME_ABI_VERSION,
      frame_reason, ku_task_control_cleanup_deadline(budget), &clock) == frame_reason);
  return KU_TASK_CONTROL_OK;
}
static void bench_drop_frame(void* raw) {
  BenchTask* task = (BenchTask*)raw;
  CHECK(task->frame_initialized);
  CHECK(ku_task_frame_0_destroy(&task->frame, sizeof(task->frame), KU_TASK_FRAME_ABI_VERSION) == KU_TASK_FRAME_OK);
  task->frame_initialized = 0;
}
static void bench_drop_payload(void* raw) {
  BenchTask* task = (BenchTask*)raw;
  CHECK(task->payload_initialized);
  ku_result_drop_int(&task->payload);
  task->payload_initialized = 0;
}
static uint32_t bench_take_payload(void* raw, void* output) {
  BenchTask* task = (BenchTask*)raw;
  CHECK(output && task->payload_initialized);
  *(KuResult_int*)output = ku_result_move_int(&task->payload);
  task->payload_initialized = 0;
  return KU_TASK_CONTROL_OK;
}
static void bench_dispose(KuTaskControlV1* control, void* raw) {
  BenchTask* task = (BenchTask*)raw;
  CHECK(control == &task->control && !task->frame_initialized && !task->payload_initialized);
  KuTaskDriverTicketV1 ticket = task->ticket;
  free(task);
  CHECK(ku_task_driver_disposed(&ticket) == KU_TASK_DRIVER_OK);
}
static const KuTaskControlOpsV1 bench_ops = {
  bench_resume, bench_cleanup, bench_drop_frame, bench_drop_payload, bench_take_payload, bench_dispose
};
static size_t bench_argument(const char* input, size_t maximum) {
  CHECK(input && *input);
  size_t result = 0;
  for (; *input; input++) {
    CHECK(*input >= '0' && *input <= '9');
    size_t digit = (size_t)(*input - '0');
    CHECK(result <= maximum / 10 && result * 10 <= maximum - digit);
    result = result * 10 + digit;
  }
  CHECK(result && result <= maximum); return result;
}
int main(int argc, char** argv) {
  CHECK(argc == 4 && KU_TASK_DRIVER_ABI_VERSION == 7u && KU_TASK_FRAME_ABI_VERSION == 4u && KU_TASK_CONTROL_ABI_VERSION == 2u);
  size_t workers = bench_argument(argv[1], BENCH_MAX_WORKERS);
  bench_count = bench_argument(argv[2], BENCH_MAX_TASKS);
  size_t quanta = bench_argument(argv[3], 65536);
  CHECK(workers <= bench_count);
  const uint64_t expected_polls = (uint64_t)bench_count * (uint64_t)quanta;
  KuTaskDriverV1* driver = (KuTaskDriverV1*)calloc(1, sizeof(*driver));
  KuTaskDriverSlotV1* slots = (KuTaskDriverSlotV1*)calloc(bench_count, sizeof(*slots));
  size_t* ring = (size_t*)calloc(bench_count, sizeof(*ring));
  CHECK(driver && slots && ring);
  size_t fixed = sizeof(*driver) + bench_count * (sizeof(*slots) + sizeof(*ring));
  size_t task_bytes = sizeof(BenchTask) + 8;
  CHECK(task_bytes <= (SIZE_MAX - fixed) / bench_count);
  KuTaskControlOwnerV1 owners[BENCH_MAX_TASKS] = {0};
  KuTaskDriverTicketV1 tickets[BENCH_MAX_TASKS] = {0};
  KuTaskDriverCleanupReceiptV1 receipts[BENCH_MAX_TASKS] = {0};
  uint64_t startup_deadline = bench_deadline(1000);
  CHECK(ku_task_driver_init(driver, sizeof(*driver), KU_TASK_DRIVER_ABI_VERSION,
      slots, bench_count, ring, bench_count, fixed + bench_count * task_bytes,
      workers, startup_deadline) == KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_wait_idle(driver, startup_deadline) == KU_TASK_DRIVER_OK);
  /* Allocate and initialize every real BUILDING frame before the measured
   * interval. Commit ramp, actual work, terminal frame drop and final idle
   * observation ARE measured; worker startup and owner teardown are not. */
  for (size_t i = 0; i < bench_count; i++) {
    KuTaskDriverTicketV1 ticket = {0};
    CHECK(ku_task_driver_reserve(driver, task_bytes, &ticket) == KU_TASK_DRIVER_OK);
    BenchTask* task = (BenchTask*)calloc(1, sizeof(*task));
    CHECK(task && ticket.slot < bench_count && !bench_tasks[ticket.slot]);
    task->ticket = ticket;
    tickets[ticket.slot] = ticket; /* Main's API ticket outlives actual context disposal. */
    uint8_t* bytes = (uint8_t*)malloc(8); CHECK(bytes); memcpy(bytes, "taskdata", 8);
    KuString owned = { bytes, 8, 8, KU_STRING_OWNED };
    int64_t count = (int64_t)quanta;
    CHECK(ku_task_control_init(&task->control, sizeof(task->control), KU_TASK_CONTROL_ABI_VERSION,
        &bench_ops, task, &owners[ticket.slot]) == KU_TASK_CONTROL_OK);
    CHECK(ku_task_frame_0_init(&task->frame, sizeof(task->frame), KU_TASK_FRAME_ABI_VERSION,
        &count, &owned) == KU_TASK_FRAME_OK);
    CHECK(!owned.ptr);
    task->frame_initialized = 1;
    bench_tasks[ticket.slot] = task;
  }
  CHECK(ku_task_driver_lock(driver) == 0);
  CHECK(driver->building == bench_count && driver->workers_waiting == workers && !driver->polls);
  bench_observing = 1;
  CHECK(ku_task_driver_unlock(driver) == 0);
  uint64_t started = ku_task_driver_now_ms(); CHECK(started != UINT64_MAX && started < UINT64_MAX - 10000u);
  uint64_t work_deadline = started + 10000u;
  for (size_t i = 0; i < bench_count; i++)
    CHECK(ku_task_driver_commit(&tickets[i], &owners[i], KU_TASK_DRIVER_START, work_deadline) == KU_TASK_DRIVER_OK);
  uint32_t waited = ku_task_driver_wait_idle(driver, work_deadline);
  uint64_t finished_at = ku_task_driver_now_ms(); CHECK(finished_at != UINT64_MAX && finished_at >= started);
  KuTaskDriverSnapshotV1 sample = {0};
  uint64_t observed_polls = 0, migrations = 0, per_worker[BENCH_MAX_WORKERS] = {0};
  CHECK(ku_task_driver_lock(driver) == 0);
  bench_observing = 0; /* All later owner/drop/shutdown polls are excluded. */
  ku_task_driver_snapshot_locked(driver, &sample);
  for (size_t i = 0; i < bench_count; i++) {
    CHECK(bench_tasks[i]->observed_polls <= UINT64_MAX - observed_polls);
    CHECK(bench_tasks[i]->migrations <= UINT64_MAX - migrations);
    observed_polls += bench_tasks[i]->observed_polls;
    migrations += bench_tasks[i]->migrations;
  }
  memcpy(per_worker, bench_worker_polls, sizeof(per_worker));
  int metrics_valid = !bench_counter_fault;
  CHECK(ku_task_driver_unlock(driver) == 0);
  int work_ok = waited == KU_TASK_DRIVER_OK && !sample.fault && !sample.clock_fault
      && sample.polls == expected_polls && !sample.queued && !sample.running && !sample.building
      && sample.terminal_held == bench_count && sample.workers_waiting == workers
      && sample.workers_created == workers && finished_at <= work_deadline;
  if (work_ok) for (size_t i = 0; i < bench_count; i++) {
    BenchTask* task = bench_tasks[i];
    CHECK(ku_task_control_status(&owners[i].lease) == KU_TASK_CONTROL_COMPLETED);
    CHECK(!task->frame_initialized && task->payload_initialized && task->payload.ok && task->payload.value == 0);
  }
#if KU_BENCH_OBSERVE
  uint64_t worker_sum = 0;
  for (size_t i = 0; i < workers; i++) { CHECK(per_worker[i] <= UINT64_MAX - worker_sum); worker_sum += per_worker[i]; }
  metrics_valid = metrics_valid && observed_polls == sample.polls && worker_sum == sample.polls
      && migrations <= (sample.polls >= bench_count ? sample.polls - bench_count : 0);
#else
  metrics_valid = metrics_valid && !observed_polls && !migrations;
#endif
  /* Same finite <=1s D on normal completion AND work observation failure.
   * Keep the genuine issued receipts until shutdown's actual reclamation. */
  const uint64_t cleanup_deadline = bench_deadline(1000);
  for (size_t i = 0; i < bench_count; i++)
    CHECK(ku_task_driver_owner_drop_receipt(&tickets[i], &owners[i], cleanup_deadline, &receipts[i]) == KU_TASK_DRIVER_OK);
  CHECK(ku_task_driver_shutdown(driver, cleanup_deadline) == KU_TASK_DRIVER_OK);
  for (size_t i = 0; i < bench_count; i++) {
    CHECK(!owners[i].lease.control);
    CHECK(ku_task_driver_cleanup_receipt_read(&receipts[i]) == KU_TASK_DRIVER_CLEANUP_ACK);
    bench_tasks[i] = NULL; /* Actual disposer has already freed it. */
  }
  KuTaskDriverSnapshotV1 clean = {0};
  CHECK(ku_task_driver_snapshot(driver, &clean) == KU_TASK_DRIVER_OK);
  CHECK(!clean.fault && !clean.clock_fault && !clean.resident && !clean.reserved_bytes && !clean.building && !clean.running && !clean.queued);
  CHECK(!clean.retiring && !clean.terminal_held && !clean.parked && !clean.workers_waiting);
  CHECK(clean.workers_exited == workers && clean.workers_created == workers);
  CHECK(ku_task_driver_join(driver, cleanup_deadline) == KU_TASK_DRIVER_OK);
  CHECK(driver->workers_joined == workers && driver->shutdown_deadline == cleanup_deadline);
  for (size_t i = 0; i < workers; i++) CHECK(driver->workers[i].joined && driver->workers[i].closed);
  CHECK(ku_task_driver_destroy(driver) == KU_TASK_DRIVER_OK);
  free(ring); free(slots); free(driver);
  FixtureLedger ledger = fixture_ledger();
  CHECK(!ledger.allocations && !ledger.bytes && !ledger.overflow);
  CHECK(ku_task_driver_now_ms() <= cleanup_deadline);
  if (!work_ok || !metrics_valid) {
    fprintf(stderr, "probe rejected after actual cleanup: work=%d metrics=%d waited=%u polls=%llu expected=%llu\n",
        work_ok, metrics_valid, waited, (unsigned long long)sample.polls, (unsigned long long)expected_polls);
    return 1;
  }
  printf("{\"observer\":%d,\"workers\":%zu,\"tasks\":%zu,\"quanta\":%zu,\"returned_polls\":%llu,\"elapsed_ms\":%llu,\"frame_bytes\":%zu,\"instance_bytes\":%zu,\"fixed_bytes\":%zu,\"migrations\":",
      KU_BENCH_OBSERVE, workers, bench_count, quanta, (unsigned long long)sample.polls,
      (unsigned long long)(finished_at-started), sizeof(KuTaskFrame_0), sizeof(BenchTask), fixed);
#if KU_BENCH_OBSERVE
  printf("%llu,\"per_worker_returned_polls\":[", (unsigned long long)migrations);
  for (size_t i = 0; i < workers; i++) printf("%s%llu", i ? "," : "", (unsigned long long)per_worker[i]);
  fputs("]", stdout);
#else
  fputs("null,\"per_worker_returned_polls\":null", stdout);
#endif
  puts(",\"ledger_zero\":true}");
  return 0;
}
"#;

#[cfg(test)]
mod row_tests {
    use super::validate_row;

    const OFF: &str = "{\"observer\":0,\"workers\":2,\"tasks\":2,\"quanta\":3,\"returned_polls\":6,\"elapsed_ms\":0,\"frame_bytes\":128,\"instance_bytes\":256,\"fixed_bytes\":4096,\"migrations\":null,\"per_worker_returned_polls\":null,\"ledger_zero\":true}\n";

    #[test]
    fn probe_row_accepts_exact_off_and_on_rows() {
        assert_eq!(validate_row(OFF.as_bytes(), 2, 2, 3, None).unwrap(), 0);
        let on = OFF
            .replace("\"observer\":0", "\"observer\":1")
            .replace("\"migrations\":null", "\"migrations\":2")
            .replace(
                "\"per_worker_returned_polls\":null",
                "\"per_worker_returned_polls\":[4,2]",
            );
        assert_eq!(validate_row(on.as_bytes(), 2, 2, 3, Some(1)).unwrap(), 1);
        assert!(validate_row(on.as_bytes(), 2, 2, 3, Some(0)).is_err());
        for (from, to) in [
            ("\"migrations\":2", "\"migrations\":5"),
            ("[4,2]", "[4,1]"),
            ("[4,2]", "[6]"),
            ("[4,2]", "[7,-1]"),
            ("[4,2]", "[4.0,2]"),
        ] {
            assert!(
                validate_row(on.replace(from, to).as_bytes(), 2, 2, 3, None).is_err(),
                "{to}"
            );
        }
    }

    #[test]
    fn probe_row_rejects_missing_malformed_and_wrong_identity_samples() {
        for text in ["", "\n", "{}\n", "null\n", "not-json\n", "\u{fffd}\n"] {
            assert!(
                validate_row(text.as_bytes(), 2, 2, 3, None).is_err(),
                "{text:?}"
            );
        }
        assert!(validate_row(&[0xff], 2, 2, 3, None).is_err());
        assert!(validate_row(format!("{OFF}{OFF}").as_bytes(), 2, 2, 3, None).is_err());
        assert!(validate_row(format!("{OFF}\n").as_bytes(), 2, 2, 3, None).is_err());
        for (from, to) in [
            ("\"observer\":0", "\"observer\":2"),
            ("\"observer\":0", "\"observer\":false"),
            ("\"workers\":2", "\"workers\":1"),
            ("\"tasks\":2", "\"tasks\":3"),
            ("\"quanta\":3", "\"quanta\":2"),
            ("\"returned_polls\":6", "\"returned_polls\":5"),
            ("\"returned_polls\":6", "\"returned_polls\":6.0"),
            ("\"elapsed_ms\":0", "\"elapsed_ms\":-1"),
            ("\"elapsed_ms\":0", "\"elapsed_ms\":10001"),
            ("\"frame_bytes\":128", "\"frame_bytes\":0"),
            ("\"instance_bytes\":256", "\"instance_bytes\":128"),
            ("\"fixed_bytes\":4096", "\"fixed_bytes\":\"4096\""),
            ("\"migrations\":null", "\"migrations\":0"),
            (
                "\"per_worker_returned_polls\":null",
                "\"per_worker_returned_polls\":[3,3]",
            ),
            ("\"ledger_zero\":true", "\"ledger_zero\":false"),
            ("\"ledger_zero\":true", "\"ledger_zero\":1"),
            ("\"ledger_zero\":true", "\"ledger_zero\":true,\"extra\":0"),
        ] {
            assert!(
                validate_row(OFF.replace(from, to).as_bytes(), 2, 2, 3, None).is_err(),
                "{to}"
            );
        }
    }
}

#[cfg(test)]
mod fixed_work_tests {
    use super::{emit, probe_native_harness as harness, validate_row};
    use std::{fs, process::Command};

    #[test]
    fn native_scheduler_probe_fixed_work_rows_and_reclamation() {
        for observe in [0, 1] {
            let directory = harness::TempDir::new("scheduler-fixed-work");
            let c_file = directory.path().join("fixed-work.c");
            emit(c_file.clone()).unwrap();
            let source = fs::read_to_string(&c_file).unwrap();
            assert_eq!(source.matches("#ifndef KU_BENCH_OBSERVE").count(), 1);
            fs::write(
                &c_file,
                source.replacen(
                    "#ifndef KU_BENCH_OBSERVE",
                    &format!("#define KU_BENCH_OBSERVE {observe}\n#ifndef KU_BENCH_OBSERVE"),
                    1,
                ),
            )
            .unwrap();
            let Some(executable) =
                harness::compile_harness(directory.path(), &c_file, "fixed-work")
            else {
                assert!(
                    std::env::var_os("CI").is_none(),
                    "native C compiler required in CI"
                );
                eprintln!("skip: no native C compiler for scheduler probe");
                return;
            };
            fs::remove_file(&c_file).unwrap();
            for workers in [1, 2, 4] {
                for quanta in [1, 2, 8] {
                    let mut command = Command::new(&executable);
                    command.current_dir(directory.path()).args([
                        workers.to_string(),
                        "4".to_owned(),
                        quanta.to_string(),
                    ]);
                    let output = harness::run_bounded(
                        &mut command,
                        harness::RUN_TIMEOUT,
                        harness::RUN_LIMITS,
                    )
                    .expect("bounded source-free fixed work");
                    assert!(
                        output.status.success(),
                        "workers={workers} quanta={quanta} observer={observe}: {}\n{}{}",
                        output.status,
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                    assert!(
                        output.stderr.is_empty(),
                        "{}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                    assert_eq!(
                        validate_row(&output.stdout, workers, 4, quanta, Some(observe)).unwrap(),
                        observe
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod private_ready_cancel_tests {
    use super::{emit, probe_native_harness as harness};
    use std::{fs, process::Command};

    fn replace_once(source: String, anchor: &str, replacement: &str) -> String {
        assert_eq!(
            source.matches(anchor).count(),
            1,
            "private-ready hook: {anchor}"
        );
        source.replacen(anchor, replacement, 1)
    }

    #[test]
    fn native_scheduler_probe_private_ready_cancel_reuses_real_cleanup() {
        for observe in [0, 1] {
            let directory = harness::TempDir::new("scheduler-private-ready-cancel");
            let c_file = directory.path().join("private-ready.c");
            emit(c_file.clone()).unwrap();
            let mut source = fs::read_to_string(&c_file).unwrap();
            source = replace_once(
                source,
                "typedef struct KuString {",
                &format!(
                    "{}\ntypedef struct KuString {{",
                    harness::NATIVE_THREAD_LIFECYCLE_HARNESS
                ),
            );
            source = replace_once(
                source,
                "#ifndef KU_BENCH_OBSERVE",
                &format!("#define KU_BENCH_OBSERVE {observe}\n#ifndef KU_BENCH_OBSERVE"),
            );
            source = replace_once(
                source,
                "static const KuTaskControlOpsV1 bench_ops = {",
                &format!("{FAULT_CALLBACKS}\nstatic const KuTaskControlOpsV1 bench_ops = {{"),
            );
            source = replace_once(source,
                "  bench_resume, bench_cleanup, bench_drop_frame, bench_drop_payload, bench_take_payload, bench_dispose",
                "  bench_fault_resume, bench_fault_cleanup, bench_fault_drop_frame, bench_fault_drop_payload, bench_take_payload, bench_fault_dispose");
            source = replace_once(source, "  CHECK(workers <= bench_count);",
                "  CHECK(workers == 1 && bench_count == 1 && quanta == 1);\n  CHECK(ku_test_event_init(&bench_fault_ready));\n  CHECK(ku_test_event_init(&bench_fault_proceed));");
            source = replace_once(source,
                "  uint32_t waited = ku_task_driver_wait_idle(driver, work_deadline);",
                "  uint32_t waited = bench_fault_cancel_private_ready(driver, &tickets[0], &owners[0]);");
            let success_check = "  if (work_ok) for (size_t i = 0; i < bench_count; i++) {\n    BenchTask* task = bench_tasks[i];\n    CHECK(ku_task_control_status(&owners[i].lease) == KU_TASK_CONTROL_COMPLETED);\n    CHECK(!task->frame_initialized && task->payload_initialized && task->payload.ok && task->payload.value == 0);\n  }";
            source = replace_once(source, success_check,
                "  CHECK(waited == KU_TASK_DRIVER_OK && sample.polls == 2);\n  bench_fault_assert_cancelled(&owners[0]);");
            source = replace_once(
                source,
                "  const uint64_t cleanup_deadline = bench_deadline(1000);",
                "  const uint64_t cleanup_deadline = bench_fault_cleanup_deadline;",
            );
            source = replace_once(source, "  if (!work_ok || !metrics_valid) {",
                "  bench_fault_finish();\n  return 0; /* Expected fault witness: NEVER emit a performance row. */\n  if (!work_ok || !metrics_valid) {");
            fs::write(&c_file, source).unwrap();
            let Some(executable) =
                harness::compile_harness(directory.path(), &c_file, "private-ready")
            else {
                assert!(
                    std::env::var_os("CI").is_none(),
                    "native C compiler required in CI"
                );
                eprintln!("skip: no native C compiler for scheduler probe cancellation");
                return;
            };
            fs::remove_file(&c_file).unwrap();
            let mut command = Command::new(executable);
            command.args(["1", "1", "1"]);
            let output =
                harness::run_bounded(&mut command, harness::RUN_TIMEOUT, harness::RUN_LIMITS)
                    .expect("bounded actual private-ready cancellation");
            assert!(
                output.status.success(),
                "status={}\n{}{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                output.stderr.is_empty(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                String::from_utf8(output.stdout).unwrap().trim(),
                "native-scheduler-probe-private-ready-cancel-ok"
            );
        }
    }

    const FAULT_CALLBACKS: &str = r#"
static KuTestEvent bench_fault_ready, bench_fault_proceed;
static uint64_t bench_fault_cleanup_deadline;
/* R2 serializes resume/cleanup/drop on one worker. Main reads these only
 * after actual all-worker idle. Dispose may run on an owner-drop caller or
 * worker; final reads follow owner API return AND actual join. Events publish D. */
static size_t bench_fault_resumes, bench_fault_payload_drops;
static size_t bench_fault_cleanups, bench_fault_frame_drops, bench_fault_disposes;
static uint32_t bench_fault_resume(void* raw) {
  CHECK(bench_fault_resumes++ == 0);
  uint32_t outcome = bench_resume(raw); /* Actual generated resume + take. */
  BenchTask* task = (BenchTask*)raw;
  CHECK(outcome == KU_TASK_CONTROL_COMPLETED && task->payload_initialized);
  CHECK(task->payload.ok && task->payload.value == 0 && task->frame_initialized);
  CHECK(task->frame.header.status == KU_TASK_FRAME_READY
      && !task->frame.header.initialized && !task->frame.header.result_initialized);
  CHECK(ku_test_event_set(&bench_fault_ready));
  CHECK(ku_test_event_wait(&bench_fault_proceed, 2000)); /* Fail-safe, not a D renewal. */
  return outcome; /* The real R2 completion CAS has not run before this return. */
}
static void bench_fault_drop_payload(void* raw) {
  CHECK(bench_fault_resumes == 1 && bench_fault_payload_drops++ == 0);
  CHECK(!bench_fault_cleanups && !bench_fault_frame_drops);
  bench_drop_payload(raw); /* Real losing-completion private Result drop. */
  CHECK(!((BenchTask*)raw)->payload_initialized);
}
static uint32_t bench_fault_cleanup(void* raw, uint32_t reason, KuTaskControlV1* budget) {
  BenchTask* task = (BenchTask*)raw;
  CHECK(bench_fault_payload_drops == 1 && bench_fault_cleanups++ == 0);
  CHECK(!bench_fault_frame_drops && reason == KU_TASK_CONTROL_CANCELLED);
  CHECK(ku_task_control_cleanup_deadline(budget) == bench_fault_cleanup_deadline);
  CHECK(task->frame.header.status == KU_TASK_FRAME_READY && !task->payload_initialized);
  uint32_t cleaned = bench_cleanup(raw, reason, budget); /* ORIGINAL BODY, not a copy. */
  CHECK(cleaned == KU_TASK_CONTROL_OK && task->frame_initialized);
  CHECK(task->frame.header.status == KU_TASK_FRAME_READY
      && !task->frame.header.initialized && !task->frame.header.result_initialized);
  return cleaned;
}
static void bench_fault_drop_frame(void* raw) {
  CHECK(bench_fault_cleanups == 1 && bench_fault_frame_drops++ == 0);
  bench_drop_frame(raw); /* Actual generated destroy; R2 still publishes terminal. */
  BenchTask* task = (BenchTask*)raw;
  CHECK(!task->frame_initialized && task->frame.header.status == KU_TASK_FRAME_DESTROYED);
}
static void bench_fault_dispose(KuTaskControlV1* control, void* raw) {
  CHECK(bench_fault_frame_drops == 1 && bench_fault_disposes == 0);
  bench_dispose(control, raw); /* Actual free + disposed returns slot/charge. */
  bench_fault_disposes++; /* No dereference after actual context disposal. */
}
static uint32_t bench_fault_cancel_private_ready(
    KuTaskDriverV1* driver, const KuTaskDriverTicketV1* ticket, KuTaskControlOwnerV1* owner) {
  CHECK(ku_test_event_wait(&bench_fault_ready, 2000));
  CHECK(ku_task_control_atomic_load(&owner->lease.control->phase) == KU_TASK_CONTROL_LIVE);
  bench_fault_cleanup_deadline = bench_deadline(1000); /* The ONE cleanup D. */
  CHECK(ku_task_driver_request_cancel(ticket, &owner->lease,
      KU_TASK_CONTROL_CANCELLED, bench_fault_cleanup_deadline) == KU_TASK_CONTROL_OK);
  CHECK(ku_task_control_atomic_load(&owner->lease.control->phase) == KU_TASK_CONTROL_REQUESTED_CANCEL);
  CHECK(ku_test_event_set(&bench_fault_proceed));
  return ku_task_driver_wait_idle(driver, bench_fault_cleanup_deadline);
}
static void bench_fault_assert_cancelled(const KuTaskControlOwnerV1* owner) {
  BenchTask* task = bench_tasks[0]; /* Owner still protects the actual context. */
  CHECK(owner->lease.control == &task->control);
  CHECK(ku_task_control_status(&owner->lease) == KU_TASK_CONTROL_CANCELLED);
  CHECK(ku_task_control_atomic_load(&task->control.payload) == KU_TASK_CONTROL_PAYLOAD_DROPPED);
  CHECK(task->control.frame_destroyed && !ku_task_control_atomic_load(&task->control.lifecycle_pin));
  CHECK(!task->frame_initialized && !task->payload_initialized
      && task->frame.header.status == KU_TASK_FRAME_DESTROYED);
  CHECK(bench_fault_resumes == 1 && bench_fault_payload_drops == 1
      && bench_fault_cleanups == 1 && bench_fault_frame_drops == 1 && !bench_fault_disposes);
}
static void bench_fault_finish(void) {
  /* Original main already required actual receipt ACK, all EXITED/joined/closed,
   * driver destroy and locked ledger0 before reaching this final callback. */
  CHECK(bench_fault_resumes == 1 && bench_fault_payload_drops == 1
      && bench_fault_cleanups == 1 && bench_fault_frame_drops == 1 && bench_fault_disposes == 1);
  CHECK(ku_test_event_destroy(&bench_fault_ready) && ku_test_event_destroy(&bench_fault_proceed));
  CHECK(ku_task_driver_now_ms() <= bench_fault_cleanup_deadline);
  puts("native-scheduler-probe-private-ready-cancel-ok");
}
"#;
}
