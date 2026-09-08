//! Internal typed frame execution, not a claim that CLI async lowering exists.
//! All subprocesses use the existing cross-platform bounded native harness.

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

fn sync_program(source: &str) -> ir::IrProgram {
    let ast = Parser::new(Lexer::new(source).lex().expect("lex sync fixture"))
        .parse_program()
        .expect("parse sync fixture");
    Checker::new().check(&ast).expect("check sync fixture");
    ir::lower_program(&ast).expect("lower sync fixture")
}

fn result(inner: IrType) -> IrType {
    IrType::Result(Box::new(inner))
}

fn slot(ty: IrType) -> TaskSlot {
    TaskSlot {
        ty: TaskSlotType::Value {
            ty,
            borrowed: false,
        },
    }
}

fn frames() -> TaskProgram {
    TaskProgram {
        functions: vec![
            TaskFunction {
                id: TaskFunctionId(0),
                name: "OwnedPending".into(),
                slots: vec![
                    slot(result(IrType::Str)),
                    slot(IrType::Str),
                    slot(IrType::Int),
                    slot(IrType::Str),
                    slot(IrType::Str),
                    slot(IrType::Str),
                ],
                parameters: vec![SlotId(0), SlotId(1)],
                entry: StateId(0),
                result: result(IrType::Str),
                states: vec![
                    TaskState {
                        operations: vec![
                            TaskOp::Init {
                                dst: SlotId(2),
                                value: TaskConstant::Int(37),
                            },
                            TaskOp::Init {
                                dst: SlotId(3),
                                value: TaskConstant::Str("temporary".into()),
                            },
                            TaskOp::Drop { slot: SlotId(3) },
                            TaskOp::Move {
                                dst: SlotId(4),
                                src: SlotId(1),
                            },
                        ],
                        terminator: TaskTerminator::Suspend {
                            resume: StateId(1),
                            cleanup: StateId(2),
                        },
                    },
                    TaskState {
                        operations: vec![TaskOp::Drop { slot: SlotId(4) }],
                        terminator: TaskTerminator::Complete { value: SlotId(0) },
                    },
                    TaskState {
                        operations: vec![TaskOp::Move {
                            dst: SlotId(5),
                            src: SlotId(4),
                        }],
                        terminator: TaskTerminator::Jump { target: StateId(3) },
                    },
                    TaskState {
                        operations: vec![
                            TaskOp::Read { slot: SlotId(5) },
                            TaskOp::Drop { slot: SlotId(5) },
                            TaskOp::Drop { slot: SlotId(0) },
                        ],
                        terminator: TaskTerminator::Terminate,
                    },
                ],
            },
            TaskFunction {
                id: TaskFunctionId(1),
                name: "PartialInitialization".into(),
                slots: vec![
                    slot(IrType::Bool),
                    slot(IrType::Str),
                    slot(result(IrType::Null)),
                ],
                parameters: vec![SlotId(0)],
                entry: StateId(0),
                result: result(IrType::Null),
                states: vec![
                    TaskState {
                        operations: vec![],
                        terminator: TaskTerminator::Branch {
                            condition: SlotId(0),
                            then_state: StateId(1),
                            else_state: StateId(2),
                        },
                    },
                    TaskState {
                        operations: vec![TaskOp::Init {
                            dst: SlotId(1),
                            value: TaskConstant::Str("hé\0中".into()),
                        }],
                        terminator: TaskTerminator::Jump { target: StateId(3) },
                    },
                    TaskState {
                        operations: vec![],
                        terminator: TaskTerminator::Jump { target: StateId(3) },
                    },
                    TaskState {
                        operations: vec![],
                        terminator: TaskTerminator::Suspend {
                            resume: StateId(4),
                            cleanup: StateId(5),
                        },
                    },
                    TaskState {
                        operations: vec![
                            TaskOp::DropIfInit { slot: SlotId(1) },
                            TaskOp::Init {
                                dst: SlotId(2),
                                value: TaskConstant::Ok(Box::new(TaskConstant::Null)),
                            },
                        ],
                        terminator: TaskTerminator::Complete { value: SlotId(2) },
                    },
                    TaskState {
                        operations: vec![TaskOp::DropIfInit { slot: SlotId(1) }],
                        terminator: TaskTerminator::Terminate,
                    },
                ],
            },
            loop_frame(),
            constant_error_frame(),
            sparse_parameters_frame(),
        ],
    }
}

fn loop_frame() -> TaskFunction {
    TaskFunction {
        id: TaskFunctionId(2),
        name: "RepeatedSuspend".into(),
        slots: vec![slot(IrType::Bool), slot(result(IrType::Int))],
        parameters: vec![],
        entry: StateId(0),
        result: result(IrType::Int),
        states: vec![
            TaskState {
                operations: vec![TaskOp::Init {
                    dst: SlotId(0),
                    value: TaskConstant::Bool(true),
                }],
                terminator: TaskTerminator::Suspend {
                    resume: StateId(1),
                    cleanup: StateId(4),
                },
            },
            TaskState {
                operations: vec![],
                terminator: TaskTerminator::Branch {
                    condition: SlotId(0),
                    then_state: StateId(2),
                    else_state: StateId(3),
                },
            },
            TaskState {
                operations: vec![TaskOp::Init {
                    dst: SlotId(0),
                    value: TaskConstant::Bool(false),
                }],
                terminator: TaskTerminator::Suspend {
                    resume: StateId(1),
                    cleanup: StateId(4),
                },
            },
            TaskState {
                operations: vec![TaskOp::Init {
                    dst: SlotId(1),
                    value: TaskConstant::Ok(Box::new(TaskConstant::Int(i64::MIN))),
                }],
                terminator: TaskTerminator::Complete { value: SlotId(1) },
            },
            TaskState {
                operations: vec![],
                terminator: TaskTerminator::Terminate,
            },
        ],
    }
}

fn constant_error_frame() -> TaskFunction {
    TaskFunction {
        id: TaskFunctionId(3),
        name: "ConstantError".into(),
        slots: vec![slot(result(IrType::Bool))],
        parameters: vec![],
        entry: StateId(0),
        result: result(IrType::Bool),
        states: vec![
            TaskState {
                operations: vec![],
                terminator: TaskTerminator::Suspend {
                    resume: StateId(1),
                    cleanup: StateId(2),
                },
            },
            TaskState {
                operations: vec![TaskOp::Init {
                    dst: SlotId(0),
                    value: TaskConstant::Err {
                        result: result(IrType::Bool),
                        domain: "task".into(),
                        code: "fixture".into(),
                        message: "hé\0中".into(),
                    },
                }],
                terminator: TaskTerminator::Complete { value: SlotId(0) },
            },
            TaskState {
                operations: vec![],
                terminator: TaskTerminator::Terminate,
            },
        ],
    }
}

fn sparse_parameters_frame() -> TaskFunction {
    let mut fields = vec![slot(IrType::Int); 64];
    fields[2] = slot(IrType::Str);
    fields[63] = slot(result(IrType::Str));
    TaskFunction {
        id: TaskFunctionId(4),
        name: "SparseReverseParameters".into(),
        slots: fields,
        parameters: vec![SlotId(63), SlotId(2)],
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
                operations: vec![TaskOp::Drop { slot: SlotId(2) }],
                terminator: TaskTerminator::Complete { value: SlotId(63) },
            },
            TaskState {
                operations: vec![
                    TaskOp::Drop { slot: SlotId(2) },
                    TaskOp::Drop { slot: SlotId(63) },
                ],
                terminator: TaskTerminator::Terminate,
            },
        ],
    }
}

#[test]
fn native_task_frame_emission_preserves_sync_output_and_fail_closed_cli_boundary() {
    let program = sync_program("fn main() {}");
    let synchronous = c::generate_c_source(&program).unwrap();
    assert_eq!(
        synchronous,
        c::generate_task_frame_c_source(&program, &TaskProgram { functions: vec![] }).unwrap()
    );
    for internal in [
        "KuTaskAdapter",
        "KuTaskInstance_",
        "KuTaskHandle_",
        "ku_task_driver_",
        "ku_task_control_",
        "ku_task_frame_",
    ] {
        assert!(!synchronous.contains(internal), "unexpected {internal}");
    }
    let tasks = frames();
    let plan = verify_and_plan(&tasks, TaskLimits::default()).unwrap();
    assert_eq!(
        plan.functions[0].slots,
        vec![SlotId(0), SlotId(1), SlotId(4)]
    );
    assert_eq!(plan.functions[2].slots, vec![SlotId(0)]);
    assert!(plan.functions[3].slots.is_empty());
    assert_eq!(plan.functions[4].slots, vec![SlotId(2), SlotId(63)]);
    let generated = c::generate_task_frame_c_source(&program, &tasks).unwrap();
    for forbidden in ["run_source", "const SOURCE", "Task.new", "task.spawn"] {
        assert!(
            !generated.contains(forbidden),
            "frame must not contain {forbidden}"
        );
    }
    assert!(generated.contains("KuTaskFrame_0"));
    assert!(generated.contains("KU_TASK_FRAME_ABI_VERSION"));

    let ast = Parser::new(
        Lexer::new("async fn Load(): int! { return ok(1) } fn main() {}")
            .lex()
            .unwrap(),
    )
    .parse_program()
    .unwrap();
    Checker::new().check(&ast).unwrap();
    assert!(ir::lower_program(&ast)
        .unwrap_err()
        .message
        .contains("async"));

    for name in [
        "ku_task_frame_0_size",
        "ku_task_control_init",
        "ku_task_driver_init",
        "KuTaskDriverV1",
        "KU_TASK_DRIVER_OK",
        "ku_task_adapter_now",
        "KuTaskAdapterClockV1",
        "KU_TASK_ADAPTER_OUT_OF_MEMORY",
        "KuTaskInstance_0",
        "KuTaskHandle_0",
        "ku_task_0_try_start",
    ] {
        let collision = sync_program(&format!("fn {name}(): int {{ return 1 }} fn main() {{}}"));
        assert!(c::generate_task_frame_c_source(&collision, &tasks)
            .unwrap_err()
            .message
            .contains("collides"));
    }
    let mut invalid = tasks;
    invalid.functions[0].states[0].terminator = TaskTerminator::Jump {
        target: StateId(999),
    };
    assert!(c::generate_task_frame_c_source(&program, &invalid).is_err());
}

#[test]
fn native_task_frame_pending_resume_cancel_and_payload_drop_execute_in_c() {
    let generated =
        c::generate_task_frame_c_source(&sync_program("fn main() {}"), &frames()).unwrap();
    assert!(generated.contains("typedef struct KuString {"));
    assert!(generated.contains("int main(void) {"));
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
    source.push_str(C_MAIN);
    let directory = TempDir::new("task-frame-v1");
    let c_file = directory.path().join("frame.c");
    fs::write(&c_file, source).expect("write generated frame harness");
    let Some(executable) = compile_harness(directory.path(), &c_file, "frame") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute the C frame fixture"
        );
        return;
    };
    fs::remove_file(&c_file).expect("remove C source before executing its artifact");
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("frame execution must be bounded");
    assert!(
        output.status.success(),
        "frame lifecycle failure:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap().replace('\r', "");
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2, "unexpected frame fixture output: {stdout}");
    assert_eq!(lines[1], "task-frame-v1-ok");
    let sizes = lines[0]
        .strip_prefix("frame-bytes:")
        .expect("target C layout sizes")
        .split(',')
        .map(|size| size.parse::<usize>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(sizes.len(), 5);
    assert!(sizes.iter().all(|size| *size > 0 && *size <= 16 * 1024));
    assert!(
        sizes[4] < sizes[0],
        "64 virtual slots must not force 64 stored fields"
    );
    eprintln!("native_task_frame_target_layout {} (frame only; excludes control block and owned heap payload)", lines[0]);
    assert!(output.stderr.is_empty());
}

#[test]
fn native_task_frame_wrap_ok_preserves_payload_without_allocating() {
    let mut tasks = TaskProgram { functions: vec![] };
    let mut cases = String::new();
    let mut local_frames = Vec::new();
    for before in [false, true] {
        for (ty, c_ty, suffix, initial, local) in [
            (IrType::Int, "int64_t", "int", "37", false),
            (IrType::Bool, "bool", "bool", "true", false),
            (IrType::Null, "uint8_t", "null", "0", false),
            (IrType::Str, "KuString", "str", "{0}", false),
            (IrType::Str, "KuString", "str", "{0}", true),
        ] {
            let id = tasks.functions.len();
            let source_slot = usize::from(local);
            let result_slot = source_slot + 1;
            let mut types = vec![slot(ty.clone())];
            let mut construction = vec![];
            if local {
                // A moved intermediate is used within one drive, never spilled.
                types.push(slot(ty.clone()));
                construction.push(TaskOp::Move {
                    dst: SlotId(1),
                    src: SlotId(0),
                });
                local_frames.push(TaskFunctionId(id));
            }
            types.push(slot(result(ty.clone())));
            construction.push(TaskOp::WrapOk {
                dst: SlotId(result_slot),
                src: SlotId(source_slot),
            });
            let mut resume = if before { vec![] } else { construction.clone() };
            if ty != IrType::Str {
                resume.push(TaskOp::Read { slot: SlotId(0) });
            }
            tasks.functions.push(TaskFunction {
                id: TaskFunctionId(id),
                name: format!("WrapOk{id}"),
                slots: types,
                parameters: vec![SlotId(0)],
                entry: StateId(0),
                result: result(ty.clone()),
                states: vec![
                    TaskState {
                        operations: if before { construction } else { vec![] },
                        terminator: TaskTerminator::Suspend {
                            resume: StateId(1),
                            cleanup: StateId(2),
                        },
                    },
                    TaskState {
                        operations: resume,
                        terminator: TaskTerminator::Complete {
                            value: SlotId(result_slot),
                        },
                    },
                    TaskState {
                        operations: (0..=result_slot)
                            .rev()
                            .map(|id| TaskOp::DropIfInit { slot: SlotId(id) })
                            .collect(),
                        terminator: TaskTerminator::Terminate,
                    },
                ],
            });
            let setup = if ty == IrType::Str {
                r#"input = ku_string_alloc(5); memcpy(input.ptr, "A\0\xe4\xb8\xad", 5);
uint8_t* original = input.ptr;"#
                    .to_string()
            } else {
                String::new()
            };
            let input_check = if ty == IrType::Str {
                "CHECK(!input.ptr && input.len == 0 && input.capacity == 0);".to_string()
            } else {
                format!("CHECK(input == {initial});")
            };
            let output_check = if ty == IrType::Str {
                r#"CHECK(output.value.ptr == original && output.value.len == 5);
CHECK(memcmp(output.value.ptr, "A\0\xe4\xb8\xad", 5) == 0);"#
                    .to_string()
            } else {
                format!("CHECK(output.value == {initial});")
            };
            cases.push_str(&format!(r#"
  for (unsigned mode = 0; mode < 4; ++mode) {{
    KuTaskFrame_{id} frame = {{0}};
    {c_ty} input = {initial};
    {setup}
    size_t allocated = ku_perf_calls;
    CHECK(ku_task_frame_{id}_init(&frame, sizeof(frame), ABI, &input) == KU_TASK_FRAME_OK);
    {input_check}
    CHECK(ku_task_frame_{id}_resume(&frame, sizeof(frame), ABI, &clock) == KU_TASK_FRAME_PENDING);
    CHECK(ku_perf_calls == allocated);
    if (mode < 2) {{
      CHECK(ku_task_frame_{id}_resume(&frame, sizeof(frame), ABI, &clock) == KU_TASK_FRAME_READY);
      if (mode == 0) {{
        KuResult_{suffix} output = {{0}};
        CHECK(ku_task_frame_{id}_take_result(&frame, sizeof(frame), ABI, &output) == KU_TASK_FRAME_OK);
        CHECK(output.ok && !output.error.domain.ptr && !output.error.code.ptr && !output.error.message.ptr);
        {output_check}
        ku_result_drop_{suffix}(&output);
      }}
    }} else {{
      uint32_t reason = mode == 2 ? KU_TASK_FRAME_CANCELLED : KU_TASK_FRAME_TIMED_OUT;
      CHECK(ku_task_frame_{id}_terminate(&frame, sizeof(frame), ABI, reason,
          mode == 2 ? 1000 : 0, &clock) == reason);
    }}
    CHECK(ku_task_frame_{id}_destroy(&frame, sizeof(frame), ABI) == KU_TASK_FRAME_OK);
    CHECK(ku_perf_calls == allocated && ku_perf_live_allocations == 0 && ku_perf_live_bytes == 0);
  }}
"#));
        }
    }
    let plan = verify_and_plan(&tasks, TaskLimits::default()).unwrap();
    for frame in &plan.functions {
        if local_frames.contains(&frame.function) {
            assert!(
                !frame.slots.contains(&SlotId(1)),
                "intermediate must exercise stack init bits"
            );
        }
    }
    let generated = c::generate_task_frame_c_source(&sync_program("fn main() {}"), &tasks).unwrap();
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
    source.push_str(r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr, "wrap check line %d\n", __LINE__); return 1; } } while (0)
#define ABI KU_TASK_FRAME_ABI_VERSION
static uint64_t wrap_now(void* context) { (void)context; return 100; }
int main(void) {
  KuTaskFrameClockV1 clock = { wrap_now, NULL };
  for (unsigned round = 0; round < 32; ++round) {
"#);
    source.push_str(&cases);
    source
        .push_str("  }\n  CHECK(!ku_perf_overflow);\n  puts(\"task-wrap-ok\");\n  return 0;\n}\n");
    let directory = TempDir::new("task-frame-wrap-ok");
    let c_file = directory.path().join("wrap.c");
    fs::write(&c_file, source).expect("write generated WrapOk fixture");
    let Some(executable) = compile_harness(directory.path(), &c_file, "wrap") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute WrapOk C fixture"
        );
        return;
    };
    fs::remove_file(&c_file).expect("remove source before native execution");
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .unwrap();
    assert!(
        output.status.success(),
        "WrapOk execution failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-wrap-ok\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn native_task_driver_thread_creation_failure_cleans_platform_initialization() {
    let generated =
        c::generate_task_frame_c_source(&sync_program("fn main() {}"), &frames()).unwrap();
    let mut source = generated
        .replacen(
            "static uint64_t ku_task_driver_now_ms(void) {",
            &format!("{DRIVER_INIT_FAULT_HOOK}\nstatic uint64_t ku_task_driver_now_ms(void) {{"),
            1,
        )
        .replacen(
            "int main(void) {",
            "static int ku_generated_main(void) {",
            1,
        );
    source.push_str(DRIVER_INIT_FAULT_MAIN);
    let directory = TempDir::new("task-driver-thread-init-failure");
    let c_file = directory.path().join("driver-init.c");
    fs::write(&c_file, source).expect("write generated driver initialization fixture");
    let Some(executable) = compile_harness(directory.path(), &c_file, "driver-init") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute driver initialization fault fixture"
        );
        return;
    };
    fs::remove_file(&c_file).expect("remove source before native execution");
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .unwrap();
    assert!(
        output.status.success(),
        "driver initialization fixture failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-driver-init-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const DRIVER_INIT_FAULT_HOOK: &str = r#"
/* Inject only the new driver's worker creation, after all platform declarations.
 * No shared harness or production fallback is changed. */
static int fixture_driver_fail_create = 1;
static unsigned fixture_driver_create_attempts;
#if defined(_WIN32)
static uintptr_t fixture_driver_beginthreadex(void* security, unsigned stack_size,
    unsigned (__stdcall *entry)(void*), void* context, unsigned flags, unsigned* id) {
  fixture_driver_create_attempts++;
  if (fixture_driver_fail_create) { errno = EAGAIN; return 0; }
  return _beginthreadex(security, stack_size, entry, context, flags, id);
}
#define _beginthreadex fixture_driver_beginthreadex
#else
#include <sched.h>
static unsigned fixture_driver_mutex_inits, fixture_driver_mutex_drops;
static unsigned fixture_driver_condition_inits, fixture_driver_condition_drops;
static int fixture_driver_mutex_init(pthread_mutex_t* mutex, const pthread_mutexattr_t* attr) {
  int result = pthread_mutex_init(mutex, attr);
  if (!result) fixture_driver_mutex_inits++;
  return result;
}
static int fixture_driver_mutex_destroy(pthread_mutex_t* mutex) {
  int result = pthread_mutex_destroy(mutex);
  if (!result) fixture_driver_mutex_drops++;
  return result;
}
static int fixture_driver_condition_init(pthread_cond_t* condition, const pthread_condattr_t* attr) {
  int result = pthread_cond_init(condition, attr);
  if (!result) fixture_driver_condition_inits++;
  return result;
}
static int fixture_driver_condition_destroy(pthread_cond_t* condition) {
  int result = pthread_cond_destroy(condition);
  if (!result) fixture_driver_condition_drops++;
  return result;
}
static int fixture_driver_pthread_create(pthread_t* thread, const pthread_attr_t* attr,
                                        void* (*entry)(void*), void* context) {
  fixture_driver_create_attempts++;
  if (fixture_driver_fail_create) return EAGAIN;
  return pthread_create(thread, attr, entry, context);
}
#define pthread_mutex_init fixture_driver_mutex_init
#define pthread_mutex_destroy fixture_driver_mutex_destroy
#define pthread_cond_init fixture_driver_condition_init
#define pthread_cond_destroy fixture_driver_condition_destroy
#define pthread_create fixture_driver_pthread_create
#endif
"#;

const DRIVER_INIT_FAULT_MAIN: &str = r#"
#define INIT_CHECK(c) do { if (!(c)) { fprintf(stderr, "driver init line %d: %s\n", __LINE__, #c); return 1; } } while (0)
int main(void) {
  KuTaskDriverV1 driver = {0};
  KuTaskDriverSlotV1 slots[2] = {0};
  size_t ring[2] = {0};
  size_t fixed = sizeof(driver) + sizeof(slots) + sizeof(ring);
  for (unsigned attempt = 0; attempt < 16; ++attempt) {
    INIT_CHECK(ku_task_driver_init(&driver, sizeof(driver), KU_TASK_DRIVER_ABI_VERSION,
        slots, 2, ring, 2, fixed) == KU_TASK_DRIVER_INTERNAL);
    INIT_CHECK(fixture_driver_create_attempts == attempt + 1);
    INIT_CHECK(ku_task_frame_zero_bytes(&driver, sizeof(driver)));
    INIT_CHECK(ku_task_frame_zero_bytes(slots, sizeof(slots)));
    INIT_CHECK(ku_task_frame_zero_bytes(ring, sizeof(ring)));
#if !defined(_WIN32)
    INIT_CHECK(fixture_driver_mutex_inits == attempt + 1);
    INIT_CHECK(fixture_driver_mutex_drops == fixture_driver_mutex_inits);
    INIT_CHECK(fixture_driver_condition_inits == attempt + 1);
    INIT_CHECK(fixture_driver_condition_drops == fixture_driver_condition_inits);
#endif
  }
  fixture_driver_fail_create = 0;
  INIT_CHECK(ku_task_driver_init(&driver, sizeof(driver), KU_TASK_DRIVER_ABI_VERSION,
      slots, 2, ring, 2, fixed) == KU_TASK_DRIVER_OK);
  INIT_CHECK(fixture_driver_create_attempts == 17);
  uint64_t deadline = ku_task_driver_now_ms() + 2000u;
  INIT_CHECK(ku_task_driver_wait_idle(&driver, deadline) == KU_TASK_DRIVER_OK);
  INIT_CHECK(ku_task_driver_shutdown(&driver, deadline) == KU_TASK_DRIVER_OK);
  uint32_t destroyed = KU_TASK_DRIVER_PENDING;
  for (unsigned attempt = 0; attempt < 4096 && destroyed == KU_TASK_DRIVER_PENDING; ++attempt) {
    destroyed = ku_task_driver_destroy(&driver);
    if (destroyed == KU_TASK_DRIVER_PENDING) {
      INIT_CHECK(ku_task_driver_now_ms() < deadline);
#if defined(_WIN32)
      SwitchToThread();
#else
      sched_yield();
#endif
    }
  }
  INIT_CHECK(destroyed == KU_TASK_DRIVER_OK);
#if !defined(_WIN32)
  INIT_CHECK(fixture_driver_mutex_inits == 17 && fixture_driver_mutex_drops == 17);
  INIT_CHECK(fixture_driver_condition_inits == 17 && fixture_driver_condition_drops == 17);
#endif
  puts("task-driver-init-ok");
  return 0;
}
"#;

const C_MAIN: &str = r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr, "frame check line %d\n", __LINE__); return 1; } } while (0)
static uint64_t fixture_now(void* context) { return *(uint64_t*)context; }
static KuString fixture_owned(const char* text) {
  size_t length = strlen(text);
  uint8_t* data = (uint8_t*)malloc(length ? length : 1);
  if (!data) abort();
  if (length) memcpy(data, text, length);
  return (KuString){ data, length, length ? length : 1, KU_STRING_OWNED };
}
static uint32_t fixture_first_poll(KuTaskFrame_0* frame, const KuTaskFrameClockV1* clock) {
  /* The first resume stack is gone before the second resume is invoked. */
  return ku_task_frame_0_resume(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION, clock);
}
typedef struct FixtureAdvancingClock {
  KuTaskFrame_0* frame;
  uint64_t now;
  size_t expected_live;
  unsigned calls;
  bool moved_before_expiry;
} FixtureAdvancingClock;
static uint64_t fixture_advancing_now(void* context) {
  FixtureAdvancingClock* clock = (FixtureAdvancingClock*)context;
  ++clock->calls;
  if (clock->calls == 2) {
    /* First cleanup block moved into stack-local slot 5 but has not dropped it. */
    clock->moved_before_expiry = clock->frame->s_4.ptr == NULL
        && !(clock->frame->header.initialized & (UINT64_C(1) << 4))
        && ku_perf_live_allocations == clock->expected_live;
  }
  return clock->now++;
}
static int fixture_cleanup_deadline_after_local_move(uint32_t reason, bool ok) {
  KuTaskFrame_0* frame = (KuTaskFrame_0*)calloc(1, sizeof(*frame));
  CHECK(frame);
  KuResult_str input = {0};
  input.ok = ok;
  if (ok) input.value = fixture_owned("result");
  else {
    input.error.domain = fixture_owned("fixture");
    input.error.code = fixture_owned("failure");
    input.error.message = fixture_owned("message");
  }
  KuString cleanup = fixture_owned("cleanup-local");
  uint64_t now = 100;
  KuTaskFrameClockV1 initial_clock = { fixture_now, &now };
  CHECK(ku_task_frame_0_init(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION, &input, &cleanup) == KU_TASK_FRAME_OK);
  CHECK(fixture_first_poll(frame, &initial_clock) == KU_TASK_FRAME_PENDING);
  FixtureAdvancingClock advancing = { frame, now, ku_perf_live_allocations, 0, false };
  KuTaskFrameClockV1 cleanup_clock = { fixture_advancing_now, &advancing };
  uint64_t deadline = now + 1;
  CHECK(ku_task_frame_0_terminate(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION, reason, deadline, &cleanup_clock) == reason);
  CHECK(advancing.calls >= 2 && advancing.calls <= 3 && advancing.moved_before_expiry);
  CHECK(frame->header.cleanup_timed_out && frame->header.cleanup_deadline_ms == deadline);
  CHECK(frame->header.initialized == 0 && !frame->header.result_initialized);
  CHECK(ku_perf_live_allocations == 1 && ku_perf_live_bytes == sizeof(*frame));
  unsigned calls = advancing.calls;
  uint32_t other_reason = reason == KU_TASK_FRAME_CANCELLED ? KU_TASK_FRAME_TIMED_OUT : KU_TASK_FRAME_CANCELLED;
  CHECK(ku_task_frame_0_terminate(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION, other_reason, deadline + 1000, &cleanup_clock) == reason);
  CHECK(ku_task_frame_0_resume(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION, &cleanup_clock) == reason);
  CHECK(advancing.calls == calls && frame->header.cleanup_deadline_ms == deadline);
  CHECK(ku_task_frame_0_destroy(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION) == KU_TASK_FRAME_OK);
  CHECK(ku_perf_live_allocations == 1 && ku_perf_live_bytes == sizeof(*frame));
  free(frame);
  CHECK(ku_perf_live_allocations == 0 && ku_perf_live_bytes == 0 && !ku_perf_overflow);
  return 0;
}
static int fixture_owned_round(int mode) {
  KuTaskFrame_0* frame = (KuTaskFrame_0*)calloc(1, ku_task_frame_0_size());
  CHECK(frame && ku_task_frame_0_size() == sizeof(*frame));
  CHECK(ku_task_frame_0_size() <= 16384 && ku_task_frame_0_align() > 0);
  CHECK((uintptr_t)frame % ku_task_frame_0_align() == 0);
  KuResult_str input = {0};
  input.ok = mode != 1 && mode != 6;
  if (input.ok) input.value = fixture_owned("payload-\xc3\xa9");
  else {
    input.error.domain = fixture_owned("fixture");
    input.error.code = fixture_owned("failure");
    input.error.message = fixture_owned("message-\xc3\xa9");
  }
  KuString cleanup = fixture_owned("cleanup");
  size_t live = ku_perf_live_allocations;
  uint64_t now = 100;
  KuTaskFrameClockV1 clock = { fixture_now, &now };
  CHECK(ku_task_frame_0_init(frame, sizeof(*frame), 0, &input, &cleanup) == KU_TASK_FRAME_ABI_MISMATCH);
  CHECK(ku_task_frame_0_init(frame, sizeof(*frame) - 1, KU_TASK_FRAME_ABI_VERSION, &input, &cleanup) == KU_TASK_FRAME_INVALID_STORAGE);
  CHECK(KU_TASK_FRAME_ABI_VERSION == 3u);
  for (uint32_t old=1u;old<3u;old++) {
    CHECK(ku_task_frame_0_init(frame, sizeof(*frame), old, &input, &cleanup) == KU_TASK_FRAME_ABI_MISMATCH);
    CHECK(ku_task_frame_zero_bytes(frame,sizeof(*frame)));
    CHECK(ku_perf_live_allocations == live && cleanup.ptr != NULL);
  }
  CHECK(ku_task_frame_0_init((char*)frame + 1, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION, &input, &cleanup) == KU_TASK_FRAME_INVALID_STORAGE);
  CHECK(ku_task_frame_0_init(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION, NULL, &cleanup) == KU_TASK_FRAME_INVALID_ARGUMENT);
  CHECK(ku_task_frame_0_init(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION, &input, (KuString*)&input) == KU_TASK_FRAME_INVALID_ARGUMENT);
  CHECK(ku_perf_live_allocations == live && cleanup.ptr != NULL);
  CHECK(ku_task_frame_0_init(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION, &input, &cleanup) == KU_TASK_FRAME_OK);
  CHECK(cleanup.ptr == NULL && input.value.ptr == NULL && input.error.message.ptr == NULL);
  CHECK(ku_task_frame_0_init(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION, &input, &cleanup) == KU_TASK_FRAME_INVALID_STATE);
  CHECK(ku_perf_live_allocations == live);

  if (mode != 4) {
    CHECK(fixture_first_poll(frame, &clock) == KU_TASK_FRAME_PENDING);
    CHECK(ku_perf_live_allocations == live);
    CHECK(frame->s_1.ptr == NULL && !(frame->header.initialized & (UINT64_C(1) << 1)));
    CHECK(frame->s_4.ptr != NULL && (frame->header.initialized & (UINT64_C(1) << 4)));
    CHECK(ku_task_frame_0_destroy(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION) == KU_TASK_FRAME_INVALID_STATE);
  }
  if (mode >= 2 && mode <= 4) {
    uint32_t reason = mode == 2 ? KU_TASK_FRAME_TIMED_OUT : KU_TASK_FRAME_CANCELLED;
    uint64_t deadline = mode == 3 ? now : now + 1000;
    CHECK(ku_task_frame_0_terminate(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION, reason, deadline, &clock) == reason);
    CHECK(ku_perf_live_allocations == 1);
    CHECK(frame->header.cleanup_timed_out == (mode == 3));
    uint64_t original_deadline = frame->header.cleanup_deadline_ms;
    CHECK(ku_task_frame_0_terminate(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION, KU_TASK_FRAME_TIMED_OUT, now + 5000, &clock) == reason);
    CHECK(frame->header.cleanup_deadline_ms <= original_deadline);
    CHECK(ku_task_frame_0_resume(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION, &clock) == reason);
    KuResult_str untouched = {0}; untouched.ok = true;
    CHECK(ku_task_frame_0_take_result(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION, &untouched) == KU_TASK_FRAME_INVALID_STATE);
    CHECK(untouched.ok);
  } else {
    CHECK(ku_task_frame_0_resume(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION, &clock) == KU_TASK_FRAME_READY);
    CHECK(ku_perf_live_allocations == live - 1);
    CHECK(ku_task_frame_0_terminate(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION, KU_TASK_FRAME_CANCELLED, now, &clock) == KU_TASK_FRAME_READY);
    if (mode >= 5) {
      CHECK(ku_task_frame_0_destroy(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION) == KU_TASK_FRAME_OK);
      CHECK(ku_perf_live_allocations == 1);
      CHECK(ku_task_frame_0_destroy(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION) == KU_TASK_FRAME_INVALID_STATE);
      free(frame);
      CHECK(ku_perf_live_allocations == 0 && ku_perf_live_bytes == 0 && !ku_perf_overflow);
      return 0;
    }
    CHECK(ku_task_frame_0_take_result(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION, &frame->s_0) == KU_TASK_FRAME_INVALID_ARGUMENT);
    KuResult_str occupied = {0}; occupied.ok = true; occupied.value = fixture_owned("occupied");
    CHECK(ku_task_frame_0_take_result(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION, &occupied) == KU_TASK_FRAME_INVALID_ARGUMENT);
    CHECK(occupied.value.len == 8);
    ku_result_drop_str(&occupied);
    KuResult_str output = {0};
    CHECK(ku_task_frame_0_take_result(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION, &output) == KU_TASK_FRAME_OK);
    CHECK(output.ok == (mode == 0));
    if (output.ok) CHECK(output.value.len == 10 && memcmp(output.value.ptr, "payload-\xc3\xa9", 10) == 0);
    else CHECK(output.error.domain.len == 7 && output.error.code.len == 7 && output.error.message.len == 10);
    KuResult_str untouched = {0}; untouched.ok = true;
    CHECK(ku_task_frame_0_take_result(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION, &untouched) == KU_TASK_FRAME_INVALID_STATE);
    CHECK(untouched.ok);
    CHECK(ku_task_frame_0_resume(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION, &clock) == KU_TASK_FRAME_READY);
    ku_result_drop_str(&output);
    CHECK(ku_perf_live_allocations == 1);
  }
  CHECK(ku_task_frame_0_destroy(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION) == KU_TASK_FRAME_OK);
  CHECK(ku_task_frame_0_destroy(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION) == KU_TASK_FRAME_INVALID_STATE);
  free(frame);
  CHECK(ku_perf_live_allocations == 0 && ku_perf_live_bytes == 0 && !ku_perf_overflow);
  return 0;
}
static int fixture_partial_round(bool condition, bool cancel) {
  KuTaskFrame_1* frame = (KuTaskFrame_1*)calloc(1, sizeof(*frame));
  CHECK(frame);
  bool argument = condition;
  uint64_t now = 1;
  KuTaskFrameClockV1 clock = { fixture_now, &now };
  CHECK(ku_task_frame_1_init(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION, &argument) == KU_TASK_FRAME_OK);
  CHECK(argument == condition);
  CHECK(ku_task_frame_1_resume(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION, &clock) == KU_TASK_FRAME_PENDING);
  CHECK(!!(frame->header.initialized & (UINT64_C(1) << 1)) == condition);
  if (condition) CHECK(frame->s_1.len == 7 && frame->s_1.ptr[3] == 0);
  if (cancel) {
    CHECK(ku_task_frame_1_terminate(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION, KU_TASK_FRAME_CANCELLED, now + 1000, &clock) == KU_TASK_FRAME_CANCELLED);
  } else {
    CHECK(ku_task_frame_1_resume(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION, &clock) == KU_TASK_FRAME_READY);
    KuResult_null output = {0};
    CHECK(ku_task_frame_1_take_result(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION, &output) == KU_TASK_FRAME_OK && output.ok);
    ku_result_drop_null(&output);
  }
  CHECK(ku_task_frame_1_destroy(frame, sizeof(*frame), KU_TASK_FRAME_ABI_VERSION) == KU_TASK_FRAME_OK);
  free(frame);
  CHECK(ku_perf_live_allocations == 0 && ku_perf_live_bytes == 0 && !ku_perf_overflow);
  return 0;
}
static int fixture_loop_and_error(void) {
  uint64_t now = 1;
  KuTaskFrameClockV1 clock = { fixture_now, &now };
  KuTaskFrame_2 loop;
  memset(&loop, 0, sizeof(loop));
  CHECK(ku_task_frame_2_init(&loop, sizeof(loop), KU_TASK_FRAME_ABI_VERSION) == KU_TASK_FRAME_OK);
  CHECK(ku_task_frame_2_resume(&loop, sizeof(loop), KU_TASK_FRAME_ABI_VERSION, &clock) == KU_TASK_FRAME_PENDING);
  CHECK(loop.s_0);
  CHECK(ku_task_frame_2_resume(&loop, sizeof(loop), KU_TASK_FRAME_ABI_VERSION, &clock) == KU_TASK_FRAME_PENDING);
  CHECK(!loop.s_0);
  CHECK(ku_task_frame_2_resume(&loop, sizeof(loop), KU_TASK_FRAME_ABI_VERSION, &clock) == KU_TASK_FRAME_READY);
  KuResult_int number = {0};
  CHECK(ku_task_frame_2_take_result(&loop, sizeof(loop), KU_TASK_FRAME_ABI_VERSION, &number) == KU_TASK_FRAME_OK);
  CHECK(number.ok && number.value == INT64_MIN);
  ku_result_drop_int(&number);
  CHECK(ku_task_frame_2_destroy(&loop, sizeof(loop), KU_TASK_FRAME_ABI_VERSION) == KU_TASK_FRAME_OK);
  for (int resumes = 1; resumes <= 2; ++resumes) {
    memset(&loop, 0, sizeof(loop));
    CHECK(ku_task_frame_2_init(&loop, sizeof(loop), KU_TASK_FRAME_ABI_VERSION) == KU_TASK_FRAME_OK);
    for (int step = 0; step < resumes; ++step)
      CHECK(ku_task_frame_2_resume(&loop, sizeof(loop), KU_TASK_FRAME_ABI_VERSION, &clock) == KU_TASK_FRAME_PENDING);
    CHECK(ku_task_frame_2_terminate(&loop, sizeof(loop), KU_TASK_FRAME_ABI_VERSION, KU_TASK_FRAME_CANCELLED, now + 1000, &clock) == KU_TASK_FRAME_CANCELLED);
    CHECK(ku_task_frame_2_resume(&loop, sizeof(loop), KU_TASK_FRAME_ABI_VERSION, &clock) == KU_TASK_FRAME_CANCELLED);
    CHECK(ku_task_frame_2_destroy(&loop, sizeof(loop), KU_TASK_FRAME_ABI_VERSION) == KU_TASK_FRAME_OK);
  }
  KuTaskFrame_3 error;
  memset(&error, 0, sizeof(error));
  CHECK(ku_task_frame_3_init(&error, sizeof(error), KU_TASK_FRAME_ABI_VERSION) == KU_TASK_FRAME_OK);
  CHECK(ku_task_frame_3_resume(&error, sizeof(error), KU_TASK_FRAME_ABI_VERSION, &clock) == KU_TASK_FRAME_PENDING);
  CHECK(ku_task_frame_3_resume(&error, sizeof(error), KU_TASK_FRAME_ABI_VERSION, &clock) == KU_TASK_FRAME_READY);
  KuResult_bool output = {0};
  CHECK(ku_task_frame_3_take_result(&error, sizeof(error), KU_TASK_FRAME_ABI_VERSION, &output) == KU_TASK_FRAME_OK);
  CHECK(!output.ok && output.error.domain.len == 4 && output.error.code.len == 7);
  CHECK(output.error.message.len == 7 && output.error.message.ptr[3] == 0);
  ku_result_drop_bool(&output);
  CHECK(ku_task_frame_3_destroy(&error, sizeof(error), KU_TASK_FRAME_ABI_VERSION) == KU_TASK_FRAME_OK);
  CHECK(ku_perf_live_allocations == 0 && ku_perf_live_bytes == 0 && !ku_perf_overflow);
  return 0;
}
static int fixture_sparse_parameters(bool cancel) {
  KuTaskFrame_4 frame;
  memset(&frame, 0, sizeof(frame));
  KuResult_str input = {0}; input.ok = true; input.value = fixture_owned("sparse");
  KuString cleanup = fixture_owned("cleanup");
  uint64_t now = 1;
  KuTaskFrameClockV1 clock = { fixture_now, &now };
  CHECK(ku_task_frame_4_init(&frame, sizeof(frame), KU_TASK_FRAME_ABI_VERSION, &input, &cleanup) == KU_TASK_FRAME_OK);
  CHECK(frame.header.initialized == ((UINT64_C(1) << 63) | (UINT64_C(1) << 2)));
  CHECK(!input.value.ptr && !cleanup.ptr && frame.s_63.value.len == 6 && frame.s_2.len == 7);
  CHECK(ku_task_frame_4_resume(&frame, sizeof(frame), KU_TASK_FRAME_ABI_VERSION, &clock) == KU_TASK_FRAME_PENDING);
  if (cancel) {
    CHECK(ku_task_frame_4_terminate(&frame, sizeof(frame), KU_TASK_FRAME_ABI_VERSION, KU_TASK_FRAME_CANCELLED, now + 1000, &clock) == KU_TASK_FRAME_CANCELLED);
  } else {
    CHECK(ku_task_frame_4_resume(&frame, sizeof(frame), KU_TASK_FRAME_ABI_VERSION, &clock) == KU_TASK_FRAME_READY);
    KuResult_str output = {0};
    CHECK(ku_task_frame_4_take_result(&frame, sizeof(frame), KU_TASK_FRAME_ABI_VERSION, &output) == KU_TASK_FRAME_OK);
    CHECK(output.ok && output.value.len == 6);
    ku_result_drop_str(&output);
  }
  CHECK(frame.header.initialized == 0);
  CHECK(ku_task_frame_4_destroy(&frame, sizeof(frame), KU_TASK_FRAME_ABI_VERSION) == KU_TASK_FRAME_OK);
  CHECK(ku_perf_live_allocations == 0 && ku_perf_live_bytes == 0 && !ku_perf_overflow);
  return 0;
}
int main(void) {
  for (int round = 0; round < 32; ++round) {
    for (int mode = 0; mode < 7; ++mode) if (fixture_owned_round(mode)) return 1;
    for (int condition = 0; condition < 2; ++condition)
      for (int cancel = 0; cancel < 2; ++cancel)
        if (fixture_partial_round(condition != 0, cancel != 0)) return 2;
    if (fixture_loop_and_error()) return 3;
    if (fixture_sparse_parameters(false) || fixture_sparse_parameters(true)) return 4;
    for (int ok = 0; ok < 2; ++ok) {
      if (fixture_cleanup_deadline_after_local_move(KU_TASK_FRAME_CANCELLED, ok != 0)) return 5;
      if (fixture_cleanup_deadline_after_local_move(KU_TASK_FRAME_TIMED_OUT, ok != 0)) return 6;
    }
  }
  printf("frame-bytes:%zu,%zu,%zu,%zu,%zu\n", ku_task_frame_0_size(), ku_task_frame_1_size(), ku_task_frame_2_size(), ku_task_frame_3_size(), ku_task_frame_4_size());
  puts("task-frame-v1-ok");
  return 0;
}
"#;
