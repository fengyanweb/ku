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

#[test]
fn native_task_driver_partial_startup_held_entries_preserve_live_storage_until_join() {
    let generated =
        c::generate_task_frame_c_source(&sync_program("fn main() {}"), &frames()).unwrap();
    // Reuse the exact successful-platform accounting from the earlier test,
    // but place only the two actual driver entrypoints behind real events.
    let mut hooks = DRIVER_INIT_FAULT_HOOK.to_owned();
    for (anchor, replacement) in [
        (
            "  uintptr_t created = _beginthreadex(security, stack_size, entry, context, flags, id);",
            "  HeldEntry* held = &held_entries[fixture_driver_round_created];\n  held->index = fixture_driver_round_created; held->entry = entry; held->context = context;\n  uintptr_t created = _beginthreadex(security, stack_size, held_driver_entry, held, flags, id);",
        ),
        (
            "  int result = pthread_create(thread, attr, entry, context);",
            "  HeldEntry* held = &held_entries[fixture_driver_round_created];\n  held->index = fixture_driver_round_created; held->entry = entry; held->context = context;\n  int result = pthread_create(thread, attr, held_driver_entry, held);",
        ),
    ] {
        assert_eq!(hooks.matches(anchor).count(), 1, "held create hook");
        hooks = hooks.replacen(anchor, replacement, 1);
    }
    let mut source = generated;
    let instrumentation = format!(
        "{}\n{ALLOCATION_HOOK}\n",
        native_harness::NATIVE_THREAD_LIFECYCLE_HARNESS
    );
    // Harness/controller definitions precede native-operation macros. Their
    // thread/events must never be counted as driver acquisitions/destructors.
    for (anchor, replacement) in [
        (
            "typedef struct KuString {",
            format!("{instrumentation}typedef struct KuString {{"),
        ),
        (
            "static uint64_t ku_task_driver_now_ms(void) {",
            format!("{HELD_ENTRY_HOOK}\n{hooks}\nstatic uint64_t ku_task_driver_now_ms(void) {{"),
        ),
        (
            "static int ku_task_driver_wait(KuTaskDriverV1* driver, uint64_t deadline) {",
            "static int ku_task_driver_wait(KuTaskDriverV1* driver, uint64_t deadline) {\n  held_observe_shutdown_wait(driver,deadline);".to_owned(),
        ),
        (
            "int main(void) {",
            "static int held_original_main(void) {".to_owned(),
        ),
    ] {
        assert_eq!(source.matches(anchor).count(), 1, "held source hook");
        source = source.replacen(anchor, &replacement, 1);
    }
    source.push_str(HELD_ENTRY_MAIN);
    let directory = TempDir::new("task-driver-held-entry");
    let c_file = directory.path().join("held-entry.c");
    fs::write(&c_file, source).expect("write held-entry lifecycle fixture");
    let Some(executable) = compile_harness(directory.path(), &c_file, "held-entry") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute the held-entry lifecycle fixture"
        );
        return;
    };
    fs::remove_file(c_file).expect("remove generated C before execution");
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("held-entry fixture must stop under its original watchdog");
    assert!(
        output.status.success(),
        "held-entry fixture failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "task-driver-held-entry-ok\n"
    );
    assert!(output.stderr.is_empty());
}

const HELD_ENTRY_HOOK: &str = r#"
#define HELD_CHECK(c) do { if (!(c)) { fprintf(stderr,"held-entry line %d: %s\n",__LINE__,#c); abort(); } } while (0)
static uint64_t ku_task_driver_now_ms(void);
static uint64_t held_deadline;
static KuTaskDriverV1* held_driver;
static KuTestEvent held_entered[2],held_release,held_shutdown_waiting;
static KU_THREAD_LOCAL int held_shutdown_caller;
#if defined(_WIN32)
typedef unsigned (__stdcall *HeldEntryFn)(void*);
#else
typedef void* (*HeldEntryFn)(void*);
#endif
typedef struct HeldEntry { HeldEntryFn entry; void* context; unsigned index; } HeldEntry;
static HeldEntry held_entries[2];
static unsigned long held_remaining(void) {
  uint64_t now=ku_task_driver_now_ms();
  HELD_CHECK(now!=UINT64_MAX && now<held_deadline);
  uint64_t left=held_deadline-now;
  HELD_CHECK(left && left<=1000u);
  return (unsigned long)left;
}
#if defined(_WIN32)
static unsigned __stdcall held_driver_entry(void* raw) {
#else
static void* held_driver_entry(void* raw) {
#endif
  const HeldEntry* entry=(const HeldEntry*)raw;
  HELD_CHECK(entry->index<2u && entry->entry && entry->context);
  HELD_CHECK(ku_test_event_set(&held_entered[entry->index]));
  HELD_CHECK(ku_test_event_wait(&held_release,held_remaining()));
  return entry->entry(entry->context); /* Actual driver entry, never fabricated exit. */
}
static void held_observe_shutdown_wait(KuTaskDriverV1* driver,uint64_t deadline) {
  if (!held_shutdown_caller) return;
  HELD_CHECK(driver==held_driver && deadline==held_deadline);
  /* Called with the real queue lock held immediately before its real OS wait.
   * The main caller subsequently acquires that lock: seeing this event alone
   * is not used as evidence the mutex has actually been released. */
  HELD_CHECK(ku_test_event_set(&held_shutdown_waiting));
}
"#;

const HELD_ENTRY_MAIN: &str = r#"
typedef struct HeldShutdown { KuTaskDriverV1* driver; uint32_t status; } HeldShutdown;
static int held_shutdown_thread(void* raw) {
  HeldShutdown* context=(HeldShutdown*)raw;
  held_shutdown_caller=1;
  context->status=ku_task_driver_shutdown(context->driver,held_deadline);
  held_shutdown_caller=0;
  return 0;
}
static void held_unreaped(KuTaskDriverV1* driver,const KuTaskDriverSlotV1* slots,
                          const size_t* ring,size_t fixed) {
  KuTaskDriverSnapshotV1 snapshot={0};
  HELD_CHECK(ku_task_driver_snapshot(driver,&snapshot)==KU_TASK_DRIVER_OK);
  /* Protect mutable metadata even when the real shutdown caller is waiting.
   * Never scan the bytes of live OS mutex/condition objects for zero. */
  HELD_CHECK(ku_task_driver_lock(driver)==0);
  HELD_CHECK(driver->initialized==KU_TASK_DRIVER_STORAGE_LIVE && driver->shutdown_deadline==held_deadline);
  HELD_CHECK(snapshot.worker_target==3u && snapshot.workers_created==2u);
  HELD_CHECK(!snapshot.workers_waiting && !snapshot.workers_exited && !snapshot.workers_joined);
  HELD_CHECK(snapshot.closing && snapshot.fault==KU_TASK_DRIVER_INTERNAL && !snapshot.clock_fault);
  HELD_CHECK(!snapshot.resident && !snapshot.building && !snapshot.queued && !snapshot.running
      && !snapshot.reserved_bytes && snapshot.fixed_bytes==fixed);
  HELD_CHECK(ku_task_frame_zero_bytes(slots,2u*sizeof(*slots))
      && ku_task_frame_zero_bytes(ring,2u*sizeof(*ring)));
  HELD_CHECK(driver->sync_resources==(KU_TASK_DRIVER_SYNC_MUTEX|KU_TASK_DRIVER_SYNC_STATE|KU_TASK_DRIVER_SYNC_WORK));
  HELD_CHECK(fixture_driver_create_attempts==3u && fixture_driver_created==2u
      && fixture_driver_round_created==2u && !fixture_driver_joined && !fixture_driver_closed);
  for (size_t i=0;i<2u;i++) {
    HELD_CHECK(driver->workers[i].driver==driver && driver->workers[i].index==i);
    HELD_CHECK(driver->workers[i].state==KU_TASK_DRIVER_WORKER_ACTIVE);
    HELD_CHECK(!driver->workers[i].joined && !driver->workers[i].closed);
#if defined(_WIN32)
    HELD_CHECK(driver->workers[i].thread==fixture_driver_threads[i].handle
        && fixture_driver_threads[i].handle && !fixture_driver_threads[i].closed);
#else
    HELD_CHECK(pthread_equal(driver->workers[i].thread,fixture_driver_threads[i].handle));
#endif
  }
#if defined(_WIN32)
  HELD_CHECK(!fixture_driver_wait_calls);
#else
  HELD_CHECK(fixture_driver_mutex_inits==1u && !fixture_driver_mutex_drops);
  HELD_CHECK(fixture_driver_condition_inits==2u && !fixture_driver_condition_drops);
#endif
  HELD_CHECK(!ku_perf_calls && !ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  HELD_CHECK(ku_task_driver_unlock(driver)==0);
}
int main(void) {
  HELD_CHECK(KU_TASK_DRIVER_ABI_VERSION==7u && KU_TASK_FRAME_ABI_VERSION==4u && KU_TASK_CONTROL_ABI_VERSION==2u);
  KuTaskDriverV1 driver={0}; KuTaskDriverSlotV1 slots[2]={0}; size_t ring[2]={0};
  size_t fixed=sizeof(driver)+sizeof(slots)+sizeof(ring);
  for (size_t i=0;i<2u;i++) HELD_CHECK(ku_test_event_init(&held_entered[i]));
  HELD_CHECK(ku_test_event_init(&held_release));
  HELD_CHECK(ku_test_event_init(&held_shutdown_waiting));
  fixture_driver_fail_after=2; held_driver=&driver;
  uint64_t now=ku_task_driver_now_ms();
  HELD_CHECK(now<UINT64_MAX-1000u); held_deadline=now+1000u;
  HELD_CHECK(ku_task_driver_init(&driver,sizeof(driver),KU_TASK_DRIVER_ABI_VERSION,
      slots,2,ring,2,fixed,3u,held_deadline)==KU_TASK_DRIVER_INTERNAL);
  for (size_t i=0;i<2u;i++) HELD_CHECK(ku_test_event_wait(&held_entered[i],held_remaining()));
  held_unreaped(&driver,slots,ring,fixed);
  KuTaskDriverTicketV1 refused={0};
  HELD_CHECK(ku_task_driver_reserve(&driver,1u,&refused)==KU_TASK_DRIVER_CLOSED);
  HELD_CHECK(ku_task_frame_zero_bytes(&refused,sizeof(refused)));
  /* No other API caller exists yet. Pending join/destroy cannot perform a
   * native join, close, zero or destructor while actual entrypoints are held. */
  HELD_CHECK(ku_task_driver_join(&driver,held_deadline)==KU_TASK_DRIVER_PENDING);
  HELD_CHECK(ku_task_driver_destroy(&driver)==KU_TASK_DRIVER_PENDING);
  held_unreaped(&driver,slots,ring,fixed);
  (void)held_remaining(); /* Reject an already-expired setup, never renew D. */
  HeldShutdown context={&driver,UINT32_MAX}; KuTestThread controller;
  HELD_CHECK(ku_test_thread_start(&controller,held_shutdown_thread,&context));
  HELD_CHECK(ku_test_event_wait(&held_shutdown_waiting,held_remaining()));
  held_unreaped(&driver,slots,ring,fixed); /* Real queue-lock acquire after wait hook. */
  HELD_CHECK(!ku_test_event_wait(&controller.finished,0u));
  (void)held_remaining();
  HELD_CHECK(ku_test_event_set(&held_release));
  HELD_CHECK(ku_test_event_wait(&controller.finished,held_remaining()));
  /* finished is already published; the helper now joins only this external
   * controller's real OS tail, using remaining original D, not a fresh 2s. */
  HELD_CHECK(ku_test_thread_join(&controller,held_remaining()));
  HELD_CHECK(!controller.outcome && context.status==KU_TASK_DRIVER_INTERNAL);
  KuTaskDriverSnapshotV1 snapshot={0};
  HELD_CHECK(ku_task_driver_snapshot(&driver,&snapshot)==KU_TASK_DRIVER_OK);
  HELD_CHECK(snapshot.worker_target==3u && snapshot.workers_created==2u && snapshot.workers_exited==2u);
  HELD_CHECK(!snapshot.workers_waiting && !snapshot.workers_joined && !snapshot.resident
      && !snapshot.reserved_bytes && !snapshot.building && !snapshot.queued && !snapshot.running);
  HELD_CHECK(snapshot.closing && snapshot.fault==KU_TASK_DRIVER_INTERNAL && !snapshot.clock_fault);
  HELD_CHECK(driver.shutdown_deadline==held_deadline && !fixture_driver_joined && !fixture_driver_closed);
  HELD_CHECK(ku_task_driver_destroy(&driver)==KU_TASK_DRIVER_PENDING);
  HELD_CHECK(ku_task_driver_join(&driver,held_deadline)==KU_TASK_DRIVER_OK);
  HELD_CHECK(driver.workers_joined==2u && fixture_driver_joined==2u && fixture_driver_closed==2u);
  for (size_t i=0;i<2u;i++)
    HELD_CHECK(driver.workers[i].joined && driver.workers[i].closed);
#if defined(_WIN32)
  unsigned waits=fixture_driver_wait_calls;
#endif
  HELD_CHECK(ku_task_driver_join(&driver,held_deadline)==KU_TASK_DRIVER_OK);
  HELD_CHECK(fixture_driver_joined==2u && fixture_driver_closed==2u);
#if defined(_WIN32)
  HELD_CHECK(fixture_driver_wait_calls==waits);
#endif
  HELD_CHECK(ku_task_driver_destroy(&driver)==KU_TASK_DRIVER_OK);
  HELD_CHECK(driver.initialized==KU_TASK_DRIVER_STORAGE_ZERO && !driver.sync_resources);
  HELD_CHECK(ku_task_frame_zero_bytes(slots,sizeof(slots)) && ku_task_frame_zero_bytes(ring,sizeof(ring)));
#if !defined(_WIN32)
  HELD_CHECK(fixture_driver_mutex_inits==1u && fixture_driver_mutex_drops==1u);
  HELD_CHECK(fixture_driver_round_mutex==&driver.mutex && fixture_driver_round_mutex_destroyed);
  HELD_CHECK(fixture_driver_condition_inits==2u && fixture_driver_condition_drops==2u
      && fixture_driver_round_conditions==2u);
  HELD_CHECK(fixture_driver_conditions[0].pointer!=fixture_driver_conditions[1].pointer);
  for (size_t i=0;i<2u;i++) {
    HELD_CHECK(fixture_driver_conditions[i].destroyed);
    HELD_CHECK(fixture_driver_conditions[i].pointer==&driver.condition
        || fixture_driver_conditions[i].pointer==&driver.work_condition);
  }
#endif
  for (size_t i=0;i<2u;i++) HELD_CHECK(ku_test_event_destroy(&held_entered[i]));
  HELD_CHECK(ku_test_event_destroy(&held_release));
  HELD_CHECK(ku_test_event_destroy(&held_shutdown_waiting));
  HELD_CHECK(!ku_perf_calls && !ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  uint64_t finished=ku_task_driver_now_ms();
  HELD_CHECK(finished!=UINT64_MAX && finished<=held_deadline);
  puts("task-driver-held-entry-ok"); return 0;
}
"#;

const DRIVER_INIT_FAULT_HOOK: &str = r#"
/* Only this driver's native operations are intercepted, after declarations.
 * All counters/handle records below have one writer: the exclusive init/join/
 * destroy caller. Worker callbacks do not touch them. Successful preceding
 * thread creations, OS waits/joins and closes always call the real platform. */
static int fixture_driver_fail_after;
static unsigned fixture_driver_create_attempts, fixture_driver_round_created;
static unsigned fixture_driver_created, fixture_driver_joined, fixture_driver_closed;
static unsigned fixture_driver_rounds;
#define INIT_HOOK_CHECK(c) do { if (!(c)) { fprintf(stderr, "driver init hook %d: %s\n", __LINE__, #c); abort(); } } while (0)
#if defined(_WIN32)
typedef struct FixtureDriverThread {
  HANDLE handle;
  unsigned joined, closed;
} FixtureDriverThread;
static FixtureDriverThread fixture_driver_threads[3];
static unsigned fixture_driver_wait_calls;
static uintptr_t fixture_driver_beginthreadex(void* security, unsigned stack_size,
    unsigned (__stdcall *entry)(void*), void* context, unsigned flags, unsigned* id) {
  fixture_driver_create_attempts++;
  if (fixture_driver_fail_after >= 0
      && fixture_driver_round_created == (unsigned)fixture_driver_fail_after) {
    errno = EAGAIN; return 0;
  }
  INIT_HOOK_CHECK(fixture_driver_round_created < 3u);
  uintptr_t created = _beginthreadex(security, stack_size, entry, context, flags, id);
  if (created) {
    fixture_driver_threads[fixture_driver_round_created].handle = (HANDLE)created;
    fixture_driver_round_created++; fixture_driver_created++;
  }
  return created;
}
static FixtureDriverThread* fixture_driver_find_thread(HANDLE handle) {
  for (unsigned index = 0; index < fixture_driver_round_created; index++)
    if (!fixture_driver_threads[index].closed && fixture_driver_threads[index].handle == handle)
      return &fixture_driver_threads[index];
  INIT_HOOK_CHECK(false); return NULL;
}
static DWORD fixture_driver_wait_thread(HANDLE handle, DWORD timeout) {
  FixtureDriverThread* thread = fixture_driver_find_thread(handle);
  INIT_HOOK_CHECK(!thread->joined && timeout != INFINITE && timeout <= 1000u);
  fixture_driver_wait_calls++;
  DWORD result = WaitForSingleObject(handle, timeout);
  if (result == WAIT_OBJECT_0) {
    thread->joined = 1; fixture_driver_joined++;
  }
  return result;
}
static BOOL fixture_driver_close_thread(HANDLE handle) {
  FixtureDriverThread* thread = fixture_driver_find_thread(handle);
  INIT_HOOK_CHECK(thread->joined && !thread->closed);
  BOOL result = CloseHandle(handle);
  if (result) { thread->closed = 1; fixture_driver_closed++; }
  return result;
}
#define _beginthreadex fixture_driver_beginthreadex
#define WaitForSingleObject fixture_driver_wait_thread
#define CloseHandle fixture_driver_close_thread
#else
typedef struct FixtureDriverThread {
  pthread_t handle;
  unsigned joined;
} FixtureDriverThread;
static FixtureDriverThread fixture_driver_threads[3];
static unsigned fixture_driver_mutex_inits, fixture_driver_mutex_drops;
static unsigned fixture_driver_condition_inits, fixture_driver_condition_drops;
static pthread_mutex_t* fixture_driver_round_mutex;
static unsigned fixture_driver_round_mutex_destroyed;
typedef struct FixtureDriverCondition {
  pthread_cond_t* pointer;
  unsigned destroyed;
} FixtureDriverCondition;
static FixtureDriverCondition fixture_driver_conditions[2];
static unsigned fixture_driver_round_conditions;
static int fixture_driver_mutex_init(pthread_mutex_t* mutex, const pthread_mutexattr_t* attr) {
  INIT_HOOK_CHECK(fixture_driver_round_mutex == NULL);
  int result = pthread_mutex_init(mutex, attr);
  if (!result) {
    fixture_driver_round_mutex = mutex; fixture_driver_mutex_inits++;
  }
  return result;
}
static int fixture_driver_mutex_destroy(pthread_mutex_t* mutex) {
  INIT_HOOK_CHECK(mutex == fixture_driver_round_mutex && !fixture_driver_round_mutex_destroyed);
  int result = pthread_mutex_destroy(mutex);
  if (!result) {
    fixture_driver_round_mutex_destroyed = 1; fixture_driver_mutex_drops++;
  }
  return result;
}
static int fixture_driver_condition_init(pthread_cond_t* condition, const pthread_condattr_t* attr) {
  INIT_HOOK_CHECK(fixture_driver_round_conditions < 2u);
  for (unsigned index = 0; index < fixture_driver_round_conditions; index++)
    INIT_HOOK_CHECK(fixture_driver_conditions[index].pointer != condition);
  int result = pthread_cond_init(condition, attr);
  if (!result) {
    fixture_driver_conditions[fixture_driver_round_conditions++].pointer = condition;
    fixture_driver_condition_inits++;
  }
  return result;
}
static int fixture_driver_condition_destroy(pthread_cond_t* condition) {
  FixtureDriverCondition* entry = NULL;
  for (unsigned index = 0; index < fixture_driver_round_conditions; index++)
    if (fixture_driver_conditions[index].pointer == condition) {
      entry = &fixture_driver_conditions[index]; break;
    }
  INIT_HOOK_CHECK(entry != NULL && !entry->destroyed);
  int result = pthread_cond_destroy(condition);
  if (!result) { entry->destroyed = 1; fixture_driver_condition_drops++; }
  return result;
}
static int fixture_driver_pthread_create(pthread_t* thread, const pthread_attr_t* attr,
                                        void* (*entry)(void*), void* context) {
  fixture_driver_create_attempts++;
  if (fixture_driver_fail_after >= 0
      && fixture_driver_round_created == (unsigned)fixture_driver_fail_after) return EAGAIN;
  INIT_HOOK_CHECK(fixture_driver_round_created < 3u);
  int result = pthread_create(thread, attr, entry, context);
  if (!result) {
    fixture_driver_threads[fixture_driver_round_created].handle = *thread;
    fixture_driver_round_created++; fixture_driver_created++;
  }
  return result;
}
static int fixture_driver_pthread_join(pthread_t handle, void** result) {
  FixtureDriverThread* thread = NULL;
  for (unsigned index = 0; index < fixture_driver_round_created; index++)
    if (!fixture_driver_threads[index].joined
        && pthread_equal(fixture_driver_threads[index].handle, handle)) {
      thread = &fixture_driver_threads[index]; break;
    }
  INIT_HOOK_CHECK(thread != NULL);
  int joined = pthread_join(handle, result);
  if (!joined) {
    thread->joined = 1; fixture_driver_joined++; fixture_driver_closed++;
  }
  return joined;
}
#define pthread_mutex_init fixture_driver_mutex_init
#define pthread_mutex_destroy fixture_driver_mutex_destroy
#define pthread_cond_init fixture_driver_condition_init
#define pthread_cond_destroy fixture_driver_condition_destroy
#define pthread_create fixture_driver_pthread_create
#define pthread_join fixture_driver_pthread_join
#endif
"#;

const DRIVER_INIT_FAULT_MAIN: &str = r#"
#define INIT_CHECK(c) do { if (!(c)) { fprintf(stderr, "driver init line %d: %s\n", __LINE__, #c); return 1; } } while (0)
static int fixture_driver_init_round(KuTaskDriverV1* driver,
                                    KuTaskDriverSlotV1* slots, size_t* ring,
                                    size_t fixed, int fail_after, unsigned expected_created) {
  unsigned before_attempts = fixture_driver_create_attempts;
  unsigned before_created = fixture_driver_created;
  unsigned before_joined = fixture_driver_joined;
  unsigned before_closed = fixture_driver_closed;
  fixture_driver_fail_after = fail_after;
  fixture_driver_round_created = 0;
  memset(fixture_driver_threads, 0, sizeof(fixture_driver_threads));
#if !defined(_WIN32)
  fixture_driver_round_mutex = NULL; fixture_driver_round_mutex_destroyed = 0;
  fixture_driver_round_conditions = 0;
  memset(fixture_driver_conditions, 0, sizeof(fixture_driver_conditions));
#endif
  uint64_t now = ku_task_driver_now_ms();
  INIT_CHECK(now != UINT64_MAX && now < UINT64_MAX - 1000u);
  uint64_t startup_deadline = now + 1000u;
  uint32_t initialized = ku_task_driver_init(driver, sizeof(*driver), KU_TASK_DRIVER_ABI_VERSION,
      slots, 2, ring, 2, fixed, 3, startup_deadline);
  INIT_CHECK(initialized == (fail_after >= 0 ? KU_TASK_DRIVER_INTERNAL : KU_TASK_DRIVER_OK));
  INIT_CHECK(driver->initialized == KU_TASK_DRIVER_STORAGE_LIVE);
  INIT_CHECK(!ku_task_frame_zero_bytes(driver, sizeof(*driver)));
  INIT_CHECK(fixture_driver_round_created == expected_created);
  INIT_CHECK(fixture_driver_created == before_created + expected_created);
  INIT_CHECK(fixture_driver_create_attempts == before_attempts + expected_created + (fail_after >= 0 ? 1u : 0u));
  INIT_CHECK(fixture_driver_joined == before_joined && fixture_driver_closed == before_closed);
  INIT_CHECK(ku_task_frame_zero_bytes(slots, 2 * sizeof(*slots)));
  INIT_CHECK(ku_task_frame_zero_bytes(ring, 2 * sizeof(*ring)));
  KuTaskDriverSnapshotV1 snapshot = {0};
  INIT_CHECK(ku_task_driver_snapshot(driver, &snapshot) == KU_TASK_DRIVER_OK);
  INIT_CHECK(snapshot.worker_target == 3u && snapshot.workers_created == expected_created);
  INIT_CHECK(snapshot.workers_waiting + snapshot.workers_exited <= expected_created);
  INIT_CHECK(!snapshot.workers_joined && !snapshot.resident && !snapshot.building
      && !snapshot.queued && !snapshot.running && !snapshot.reserved_bytes);
  INIT_CHECK(snapshot.fixed_bytes == fixed && !snapshot.clock_fault);
  if (fail_after >= 0) {
    INIT_CHECK(snapshot.closing && snapshot.fault == KU_TASK_DRIVER_INTERNAL);
    INIT_CHECK(ku_task_driver_lock(driver) == 0);
    uint64_t observed_deadline = driver->shutdown_deadline;
    INIT_CHECK(ku_task_driver_unlock(driver) == 0);
    INIT_CHECK(observed_deadline == startup_deadline);
    KuTaskDriverTicketV1 rejected = {0};
    INIT_CHECK(ku_task_driver_reserve(driver, 1, &rejected) == KU_TASK_DRIVER_CLOSED);
    INIT_CHECK(ku_task_frame_zero_bytes(&rejected, sizeof(rejected)));
  } else {
    INIT_CHECK(!snapshot.closing && !snapshot.fault);
    INIT_CHECK(ku_task_driver_wait_idle(driver, startup_deadline) == KU_TASK_DRIVER_OK);
  }
  /* A sticky initialization fault is preserved even when real cleanup succeeds.
   * This one D is reused across logical exit, every native join and destruction. */
  uint32_t shutdown = ku_task_driver_shutdown(driver, startup_deadline);
  INIT_CHECK(shutdown == (fail_after >= 0 ? KU_TASK_DRIVER_INTERNAL : KU_TASK_DRIVER_OK));
  INIT_CHECK(ku_task_driver_snapshot(driver, &snapshot) == KU_TASK_DRIVER_OK);
  INIT_CHECK(snapshot.workers_created == expected_created && snapshot.workers_exited == expected_created);
  INIT_CHECK(!snapshot.workers_waiting && !snapshot.workers_joined
      && !snapshot.resident && !snapshot.building && !snapshot.queued
      && !snapshot.running && !snapshot.reserved_bytes);
  INIT_CHECK(ku_task_driver_lock(driver) == 0);
  uint64_t stopped_deadline = driver->shutdown_deadline;
  INIT_CHECK(ku_task_driver_unlock(driver) == 0);
  INIT_CHECK(stopped_deadline == startup_deadline);
  for (unsigned index = 0; index < expected_created; index++) {
    INIT_CHECK(driver->workers[index].state == KU_TASK_DRIVER_WORKER_EXITED);
    INIT_CHECK(!driver->workers[index].joined && !driver->workers[index].closed);
  }
  if (expected_created) INIT_CHECK(ku_task_driver_destroy(driver) == KU_TASK_DRIVER_PENDING);
  INIT_CHECK(ku_task_driver_join(driver, startup_deadline) == KU_TASK_DRIVER_OK);
  INIT_CHECK(fixture_driver_joined == before_joined + expected_created);
  INIT_CHECK(fixture_driver_closed == before_closed + expected_created);
  INIT_CHECK(driver->workers_joined == expected_created);
  for (unsigned index = 0; index < expected_created; index++)
    INIT_CHECK(driver->workers[index].joined && driver->workers[index].closed);
  /* Idempotent reaping must not call native wait/join/close a second time. */
#if defined(_WIN32)
  unsigned before_waits = fixture_driver_wait_calls;
#endif
  INIT_CHECK(ku_task_driver_join(driver, startup_deadline) == KU_TASK_DRIVER_OK);
  INIT_CHECK(fixture_driver_joined == before_joined + expected_created);
  INIT_CHECK(fixture_driver_closed == before_closed + expected_created);
#if defined(_WIN32)
  INIT_CHECK(fixture_driver_wait_calls == before_waits);
#endif
  INIT_CHECK(ku_task_driver_destroy(driver) == KU_TASK_DRIVER_OK);
  INIT_CHECK(driver->initialized == KU_TASK_DRIVER_STORAGE_ZERO && !driver->sync_resources);
  INIT_CHECK(ku_task_frame_zero_bytes(slots, 2 * sizeof(*slots)));
  INIT_CHECK(ku_task_frame_zero_bytes(ring, 2 * sizeof(*ring)));
  fixture_driver_rounds++;
#if !defined(_WIN32)
  INIT_CHECK(fixture_driver_mutex_inits == fixture_driver_rounds);
  INIT_CHECK(fixture_driver_mutex_drops == fixture_driver_mutex_inits);
  INIT_CHECK(fixture_driver_condition_inits == 2u * fixture_driver_rounds);
  INIT_CHECK(fixture_driver_condition_drops == fixture_driver_condition_inits);
  INIT_CHECK(fixture_driver_round_mutex == &driver->mutex && fixture_driver_round_mutex_destroyed);
  INIT_CHECK(fixture_driver_round_conditions == 2u);
  INIT_CHECK(fixture_driver_conditions[0].pointer != fixture_driver_conditions[1].pointer);
  for (unsigned index = 0; index < 2u; index++) {
    INIT_CHECK(fixture_driver_conditions[index].destroyed);
    INIT_CHECK(fixture_driver_conditions[index].pointer == &driver->condition
        || fixture_driver_conditions[index].pointer == &driver->work_condition);
  }
#endif
  /* This finite test must actually finish inside its one original D. It is
   * an observed test gate, not a hard real-time guarantee for POSIX OS joining. */
  uint64_t finished_at = ku_task_driver_now_ms();
  INIT_CHECK(finished_at != UINT64_MAX && finished_at <= startup_deadline);
  return 0;
}
int main(void) {
  INIT_CHECK(KU_TASK_DRIVER_ABI_VERSION == 7u);
  INIT_CHECK(KU_TASK_FRAME_ABI_VERSION == 4u && KU_TASK_CONTROL_ABI_VERSION == 2u);
  KuTaskDriverV1 driver = {0};
  KuTaskDriverSlotV1 slots[2] = {0};
  size_t ring[2] = {0};
  size_t fixed = sizeof(driver) + sizeof(slots) + sizeof(ring);
  for (unsigned attempt = 0; attempt < 16; ++attempt) {
    INIT_CHECK(!fixture_driver_init_round(&driver, slots, ring, fixed, 0, 0));
    /* Only the caller resets storage, and only after all actual native resources
     * have been destroyed. Init failure itself must never clear a live group. */
    memset(&driver, 0, sizeof(driver));
  }
  INIT_CHECK(!fixture_driver_init_round(&driver, slots, ring, fixed, 1, 1));
  memset(&driver, 0, sizeof(driver));
  INIT_CHECK(!fixture_driver_init_round(&driver, slots, ring, fixed, 2, 2));
  memset(&driver, 0, sizeof(driver));
  INIT_CHECK(!fixture_driver_init_round(&driver, slots, ring, fixed, -1, 3));
  INIT_CHECK(fixture_driver_rounds == 19u && fixture_driver_create_attempts == 24u);
  INIT_CHECK(fixture_driver_created == 6u && fixture_driver_joined == 6u
      && fixture_driver_closed == 6u);
#if !defined(_WIN32)
  INIT_CHECK(fixture_driver_mutex_inits == 19u && fixture_driver_mutex_drops == 19u);
  INIT_CHECK(fixture_driver_condition_inits == 38u && fixture_driver_condition_drops == 38u);
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
  CHECK(KU_TASK_FRAME_ABI_VERSION == 4u);
  for (uint32_t old=1u;old<KU_TASK_FRAME_ABI_VERSION;old++) {
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

#[cfg(unix)]
#[test]
fn native_task_driver_posix_sync_resource_failures_preserve_exact_reaping_state() {
    let generated =
        c::generate_task_frame_c_source(&sync_program("fn main() {}"), &frames()).unwrap();
    let entry = "static uint64_t ku_task_driver_now_ms(void) {";
    assert_eq!(generated.matches(entry).count(), 1);
    assert_eq!(generated.matches("int main(void) {").count(), 1);
    let hooks = format!("{DRIVER_INIT_FAULT_HOOK}\n{POSIX_SYNC_FAULT_HOOK}\n{entry}");
    let mut source = generated.replacen(entry, &hooks, 1).replacen(
        "int main(void) {",
        "static int ku_generated_main(void) {",
        1,
    );
    source.push_str(POSIX_SYNC_FAULT_MAIN);
    let directory = TempDir::new("task-driver-posix-sync-failures");
    let c_file = directory.path().join("sync-failures.c");
    fs::write(&c_file, source).expect("write POSIX resource failure matrix");
    let Some(executable) = compile_harness(directory.path(), &c_file, "sync-failures") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute the POSIX resource failure matrix"
        );
        return;
    };
    fs::remove_file(&c_file).expect("remove source before native resource execution");
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("POSIX resource matrix must remain bounded");
    assert!(
        output.status.success(),
        "POSIX resource matrix failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let expected = if cfg!(target_vendor = "apple") {
        "task-driver-posix-sync-ok rounds=7 created=8 joined=8\n"
    } else {
        "task-driver-posix-sync-ok rounds=10 created=8 joined=8\n"
    };
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        expected
    );
    assert!(output.stderr.is_empty());
}

#[cfg(unix)]
const POSIX_SYNC_FAULT_HOOK: &str = r#"
#if defined(_WIN32)
#error "POSIX-only witness: Windows SRW/condition resources have no destructor"
#endif
/* Layer one pre-call synthetic failure over the existing actual-resource and
 * actual-pthread records. No successful native operation or join is faked. */
#undef pthread_mutex_init
#undef pthread_mutex_destroy
#undef pthread_cond_init
#undef pthread_cond_destroy
enum { SYNC_M, SYNC_A, SYNC_S, SYNC_W, SYNC_RESOURCE_COUNT };
enum {
  SYNC_NO_FAULT, SYNC_M_INIT, SYNC_A_INIT, SYNC_A_CLOCK, SYNC_S_INIT, SYNC_W_INIT,
  SYNC_A_DROP, SYNC_W_DROP, SYNC_S_DROP, SYNC_M_DROP
};
typedef struct FixtureSyncResource {
  void* pointer;
  unsigned init_attempts, inits, drop_attempts, drops;
} FixtureSyncResource;
static FixtureSyncResource fixture_sync_resources[SYNC_RESOURCE_COUNT];
static KuTaskDriverV1* fixture_sync_driver;
static unsigned fixture_sync_fault, fixture_sync_hits, fixture_sync_teardown;
static unsigned fixture_sync_clock_attempts, fixture_sync_clocks;
static const uint32_t fixture_sync_bits[SYNC_RESOURCE_COUNT] = {
  KU_TASK_DRIVER_SYNC_MUTEX, KU_TASK_DRIVER_SYNC_ATTRIBUTES,
  KU_TASK_DRIVER_SYNC_STATE, KU_TASK_DRIVER_SYNC_WORK
};
static int fixture_sync_fail(unsigned operation) {
  if (fixture_sync_fault != operation) return 0;
  INIT_HOOK_CHECK(!fixture_sync_hits);
  fixture_sync_hits++; fixture_sync_fault = SYNC_NO_FAULT; return 1;
}
static FixtureSyncResource* fixture_sync_entry(unsigned kind, void* pointer, int dropping) {
  FixtureSyncResource* resource = &fixture_sync_resources[kind];
  INIT_HOOK_CHECK(pointer == resource->pointer);
  if (dropping) {
    INIT_HOOK_CHECK(resource->inits == 1 && !resource->drops);
    resource->drop_attempts++;
  } else {
    INIT_HOOK_CHECK(!resource->inits && !resource->init_attempts);
    resource->init_attempts++;
  }
  return resource;
}
static int fixture_sync_mutex_init(pthread_mutex_t* mutex, const pthread_mutexattr_t* attr) {
  FixtureSyncResource* resource = fixture_sync_entry(SYNC_M,mutex,0);
  if (fixture_sync_fail(SYNC_M_INIT)) return EAGAIN;
  int result = fixture_driver_mutex_init(mutex,attr);
  if (!result) resource->inits++;
  return result;
}
static int fixture_sync_mutex_destroy(pthread_mutex_t* mutex) {
  FixtureSyncResource* resource = fixture_sync_entry(SYNC_M,mutex,1);
  if (fixture_sync_fail(SYNC_M_DROP)) return EBUSY;
  int result = fixture_driver_mutex_destroy(mutex);
  if (!result) resource->drops++;
  return result;
}
static unsigned fixture_sync_condition_kind(pthread_cond_t* condition) {
  if (condition == &fixture_sync_driver->condition) return SYNC_S;
  INIT_HOOK_CHECK(condition == &fixture_sync_driver->work_condition); return SYNC_W;
}
static int fixture_sync_condition_init(pthread_cond_t* condition, const pthread_condattr_t* attr) {
  unsigned kind = fixture_sync_condition_kind(condition);
  FixtureSyncResource* resource = fixture_sync_entry(kind,condition,0);
  if (fixture_sync_fail(kind == SYNC_S ? SYNC_S_INIT : SYNC_W_INIT)) return EAGAIN;
  int result = fixture_driver_condition_init(condition,attr);
  if (!result) resource->inits++;
  return result;
}
static int fixture_sync_condition_destroy(pthread_cond_t* condition) {
  unsigned kind = fixture_sync_condition_kind(condition);
  FixtureSyncResource* resource = fixture_sync_entry(kind,condition,1);
  if (fixture_sync_fail(kind == SYNC_S ? SYNC_S_DROP : SYNC_W_DROP)) return EBUSY;
  int result = fixture_driver_condition_destroy(condition);
  if (!result) resource->drops++;
  return result;
}
#if !defined(__APPLE__)
static int fixture_sync_attr_init(pthread_condattr_t* attr) {
  FixtureSyncResource* resource = fixture_sync_entry(SYNC_A,attr,0);
  if (fixture_sync_fail(SYNC_A_INIT)) return EAGAIN;
  int result = pthread_condattr_init(attr);
  if (!result) resource->inits++;
  return result;
}
static int fixture_sync_attr_setclock(pthread_condattr_t* attr, clockid_t clock) {
  FixtureSyncResource* resource = &fixture_sync_resources[SYNC_A];
  INIT_HOOK_CHECK(attr == resource->pointer && resource->inits == 1 && !resource->drops);
  INIT_HOOK_CHECK(clock == CLOCK_MONOTONIC); fixture_sync_clock_attempts++;
  if (fixture_sync_fail(SYNC_A_CLOCK)) return EINVAL;
  int result = pthread_condattr_setclock(attr,clock);
  if (!result) fixture_sync_clocks++;
  return result;
}
static int fixture_sync_attr_destroy(pthread_condattr_t* attr) {
  FixtureSyncResource* resource = fixture_sync_entry(SYNC_A,attr,1);
  if (fixture_sync_fail(SYNC_A_DROP)) return EBUSY;
  int result = pthread_condattr_destroy(attr);
  if (!result) resource->drops++;
  return result;
}
#endif
/* These checks only READ records while workers live. Init records precede
 * pthread_create; teardown/drops start after every real pthread_join. Thus
 * no unsynchronized counter writes are introduced on worker lock paths. */
static void fixture_sync_check_mutex(pthread_mutex_t* mutex) {
  const FixtureSyncResource* resource = &fixture_sync_resources[SYNC_M];
  INIT_HOOK_CHECK(mutex == resource->pointer && resource->inits == 1 && !resource->drops);
  INIT_HOOK_CHECK(!fixture_sync_teardown
      || fixture_sync_driver->initialized != KU_TASK_DRIVER_STORAGE_REAPING);
}
static int fixture_sync_mutex_lock(pthread_mutex_t* mutex) {
  fixture_sync_check_mutex(mutex); return pthread_mutex_lock(mutex);
}
static int fixture_sync_mutex_unlock(pthread_mutex_t* mutex) {
  fixture_sync_check_mutex(mutex); return pthread_mutex_unlock(mutex);
}
#define pthread_mutex_init fixture_sync_mutex_init
#define pthread_mutex_destroy fixture_sync_mutex_destroy
#define pthread_mutex_lock fixture_sync_mutex_lock
#define pthread_mutex_unlock fixture_sync_mutex_unlock
#define pthread_cond_init fixture_sync_condition_init
#define pthread_cond_destroy fixture_sync_condition_destroy
#if !defined(__APPLE__)
#define pthread_condattr_init fixture_sync_attr_init
#define pthread_condattr_setclock fixture_sync_attr_setclock
#define pthread_condattr_destroy fixture_sync_attr_destroy
#endif
"#;

#[cfg(unix)]
const POSIX_SYNC_FAULT_MAIN: &str = r#"
#define SYNC_CHECK(c) INIT_HOOK_CHECK(c)
static void fixture_sync_bits_match(KuTaskDriverV1* driver, uint32_t expected) {
  uint32_t actual_live = 0;
  for (unsigned kind=0;kind<SYNC_RESOURCE_COUNT;kind++) {
    FixtureSyncResource* resource = &fixture_sync_resources[kind];
    SYNC_CHECK(resource->inits <= 1 && resource->drops <= resource->inits);
    if (resource->inits && !resource->drops) actual_live |= fixture_sync_bits[kind];
  }
  SYNC_CHECK(driver->sync_resources == expected && actual_live == expected);
}
static void fixture_sync_reject_ordinary(KuTaskDriverV1* driver, uint64_t deadline) {
  SYNC_CHECK(driver->initialized == KU_TASK_DRIVER_STORAGE_REAPING);
  SYNC_CHECK(ku_task_driver_check(driver) == KU_TASK_DRIVER_INVALID_STATE);
  KuTaskDriverSnapshotV1 snapshot, before;
  memset(&snapshot,0xa5,sizeof(snapshot)); memcpy(&before,&snapshot,sizeof(before));
  SYNC_CHECK(ku_task_driver_snapshot(driver,&snapshot) == KU_TASK_DRIVER_INVALID_STATE);
  SYNC_CHECK(!memcmp(&snapshot,&before,sizeof(snapshot)));
  KuTaskDriverTicketV1 rejected = {0};
  SYNC_CHECK(ku_task_driver_reserve(driver,1,&rejected) == KU_TASK_DRIVER_INVALID_STATE);
  SYNC_CHECK(ku_task_frame_zero_bytes(&rejected,sizeof(rejected)));
  SYNC_CHECK(ku_task_driver_shutdown(driver,deadline) == KU_TASK_DRIVER_INVALID_STATE);
}
static void fixture_sync_round(KuTaskDriverV1* driver, KuTaskDriverSlotV1* slots,
                               size_t* ring, size_t fixed, unsigned operation,
                               uint32_t failure_bits, unsigned expected_created) {
  fixture_sync_driver = driver; fixture_sync_fault = operation;
  fixture_sync_hits = fixture_sync_teardown = fixture_sync_clock_attempts = fixture_sync_clocks = 0;
  memset(fixture_sync_resources,0,sizeof(fixture_sync_resources));
  fixture_sync_resources[SYNC_M].pointer = &driver->mutex;
  fixture_sync_resources[SYNC_S].pointer = &driver->condition;
  fixture_sync_resources[SYNC_W].pointer = &driver->work_condition;
#if !defined(__APPLE__)
  fixture_sync_resources[SYNC_A].pointer = &driver->condition_attributes;
#endif
  fixture_driver_fail_after = -1;
  fixture_driver_create_attempts = fixture_driver_round_created = 0;
  fixture_driver_created = fixture_driver_joined = fixture_driver_closed = 0;
  fixture_driver_mutex_inits = fixture_driver_mutex_drops = 0;
  fixture_driver_condition_inits = fixture_driver_condition_drops = 0;
  fixture_driver_round_mutex = NULL; fixture_driver_round_mutex_destroyed = 0;
  fixture_driver_round_conditions = 0;
  memset(fixture_driver_threads,0,sizeof(fixture_driver_threads));
  memset(fixture_driver_conditions,0,sizeof(fixture_driver_conditions));
  uint64_t now = ku_task_driver_now_ms();
  SYNC_CHECK(now != UINT64_MAX && now < UINT64_MAX-1000u);
  const uint64_t deadline = now+1000u;
  uint32_t initialized = ku_task_driver_init(driver,sizeof(*driver),KU_TASK_DRIVER_ABI_VERSION,
      slots,1,ring,1,fixed,2,deadline);
  SYNC_CHECK(initialized == (expected_created ? KU_TASK_DRIVER_OK : KU_TASK_DRIVER_INTERNAL));
  SYNC_CHECK(fixture_driver_round_created == expected_created
      && fixture_driver_create_attempts == expected_created
      && fixture_driver_created == expected_created && !fixture_driver_joined);
  if (!expected_created) {
    /* No handle was created: join below is resource-only readiness, not a
     * claim that a requested two-worker group started or completed. */
    SYNC_CHECK(fixture_sync_hits == 1 && driver->initialized == KU_TASK_DRIVER_STORAGE_REAPING
        && driver->workers_created == 0 && driver->worker_target == 2
        && driver->closing && driver->fault == KU_TASK_DRIVER_INTERNAL
        && driver->shutdown_deadline == deadline);
    fixture_sync_bits_match(driver,failure_bits);
    fixture_sync_teardown = 1;
    fixture_sync_reject_ordinary(driver,deadline);
  } else {
    SYNC_CHECK(driver->initialized == KU_TASK_DRIVER_STORAGE_LIVE && !fixture_sync_hits);
    fixture_sync_bits_match(driver,KU_TASK_DRIVER_SYNC_MUTEX|KU_TASK_DRIVER_SYNC_STATE|KU_TASK_DRIVER_SYNC_WORK);
    SYNC_CHECK(ku_task_driver_wait_idle(driver,deadline) == KU_TASK_DRIVER_OK);
    SYNC_CHECK(ku_task_driver_shutdown(driver,deadline) == KU_TASK_DRIVER_OK);
    KuTaskDriverSnapshotV1 stopped = {0};
    SYNC_CHECK(ku_task_driver_snapshot(driver,&stopped) == KU_TASK_DRIVER_OK);
    SYNC_CHECK(stopped.workers_created == 2 && stopped.workers_exited == 2
        && !stopped.workers_waiting && !stopped.resident && !stopped.building
        && !stopped.queued && !stopped.running && !stopped.reserved_bytes && !stopped.fault);
    SYNC_CHECK(ku_task_driver_lock(driver) == 0);
    SYNC_CHECK(driver->shutdown_deadline == deadline);
    SYNC_CHECK(ku_task_driver_unlock(driver) == 0);
    SYNC_CHECK(ku_task_driver_destroy(driver) == KU_TASK_DRIVER_PENDING);
  }
  SYNC_CHECK(ku_task_driver_join(driver,deadline) == KU_TASK_DRIVER_OK);
  SYNC_CHECK(fixture_driver_joined == expected_created && fixture_driver_closed == expected_created);
  SYNC_CHECK(ku_task_driver_join(driver,deadline) == KU_TASK_DRIVER_OK);
  SYNC_CHECK(fixture_driver_joined == expected_created && fixture_driver_closed == expected_created);
  fixture_sync_teardown = 1;
  uint32_t destroyed = ku_task_driver_destroy(driver);
  if (expected_created && operation != SYNC_NO_FAULT) {
    SYNC_CHECK(destroyed == KU_TASK_DRIVER_INTERNAL && fixture_sync_hits == 1);
    fixture_sync_bits_match(driver,failure_bits);
    fixture_sync_reject_ordinary(driver,deadline);
    /* REAPING retry must not lock even a remaining live mutex. Already
     * destroyed condition pointers are rejected before any real destructor. */
    SYNC_CHECK(ku_task_driver_join(driver,deadline) == KU_TASK_DRIVER_OK);
    SYNC_CHECK(fixture_driver_joined == expected_created && fixture_driver_closed == expected_created);
    destroyed = ku_task_driver_destroy(driver);
  }
  SYNC_CHECK(destroyed == KU_TASK_DRIVER_OK && driver->initialized == KU_TASK_DRIVER_STORAGE_ZERO);
  fixture_sync_bits_match(driver,0);
  SYNC_CHECK(fixture_sync_hits == (operation == SYNC_NO_FAULT ? 0u : 1u) && !fixture_sync_fault);
  for (unsigned kind=0;kind<SYNC_RESOURCE_COUNT;kind++) {
    FixtureSyncResource* resource = &fixture_sync_resources[kind];
    unsigned init_failed = (kind == SYNC_M && operation == SYNC_M_INIT)
        || (kind == SYNC_A && operation == SYNC_A_INIT)
        || (kind == SYNC_S && operation == SYNC_S_INIT)
        || (kind == SYNC_W && operation == SYNC_W_INIT);
    unsigned drop_failed = (kind == SYNC_M && operation == SYNC_M_DROP)
        || (kind == SYNC_A && operation == SYNC_A_DROP)
        || (kind == SYNC_S && operation == SYNC_S_DROP)
        || (kind == SYNC_W && operation == SYNC_W_DROP);
    SYNC_CHECK(resource->inits == resource->drops
        && resource->init_attempts == resource->inits+init_failed
        && resource->drop_attempts == resource->drops+drop_failed);
  }
  SYNC_CHECK(fixture_sync_clock_attempts == fixture_sync_clocks+(operation == SYNC_A_CLOCK));
  SYNC_CHECK(fixture_driver_mutex_inits == fixture_sync_resources[SYNC_M].inits
      && fixture_driver_mutex_drops == fixture_driver_mutex_inits);
  if (fixture_driver_mutex_inits)
    SYNC_CHECK(fixture_driver_round_mutex == &driver->mutex && fixture_driver_round_mutex_destroyed);
  SYNC_CHECK(fixture_driver_condition_inits == fixture_sync_resources[SYNC_S].inits+fixture_sync_resources[SYNC_W].inits
      && fixture_driver_condition_drops == fixture_driver_condition_inits
      && fixture_driver_round_conditions == fixture_driver_condition_inits);
  for (unsigned i=0;i<fixture_driver_round_conditions;i++)
    SYNC_CHECK(fixture_driver_conditions[i].destroyed
        && (fixture_driver_conditions[i].pointer == &driver->condition
            || fixture_driver_conditions[i].pointer == &driver->work_condition));
  SYNC_CHECK(driver->workers_joined == expected_created);
  for (unsigned i=0;i<expected_created;i++)
    SYNC_CHECK(driver->workers[i].state == KU_TASK_DRIVER_WORKER_EXITED
        && driver->workers[i].joined && driver->workers[i].closed && fixture_driver_threads[i].joined);
  SYNC_CHECK(ku_task_frame_zero_bytes(slots,sizeof(*slots)) && ku_task_frame_zero_bytes(ring,sizeof(*ring)));
  now = ku_task_driver_now_ms();
  SYNC_CHECK(now != UINT64_MAX && now <= deadline);
}
int main(void) {
  const uint32_t m = KU_TASK_DRIVER_SYNC_MUTEX, s = KU_TASK_DRIVER_SYNC_STATE, w = KU_TASK_DRIVER_SYNC_WORK;
#if defined(__APPLE__)
  const uint32_t a = 0;
#else
  const uint32_t a = KU_TASK_DRIVER_SYNC_ATTRIBUTES;
#endif
  struct { unsigned operation; uint32_t failure_bits; unsigned created; } cases[] = {
    {SYNC_M_INIT,0,0},
#if !defined(__APPLE__)
    {SYNC_A_INIT,m,0}, {SYNC_A_CLOCK,m|a,0},
#endif
    {SYNC_S_INIT,m|a,0}, {SYNC_W_INIT,m|a|s,0},
#if !defined(__APPLE__)
    /* Attributes are destroyed at init's tail BEFORE pthread creation. */
    {SYNC_A_DROP,m|a|s|w,0},
#endif
    {SYNC_NO_FAULT,0,2}, {SYNC_W_DROP,m|s|w,2}, {SYNC_S_DROP,m|s,2}, {SYNC_M_DROP,m,2}
  };
  KuTaskDriverV1 driver = {0};
  KuTaskDriverSlotV1 slots[1] = {{0}};
  size_t ring[1] = {0};
  const size_t fixed = sizeof(driver)+sizeof(slots)+sizeof(ring);
  unsigned rounds = (unsigned)(sizeof(cases)/sizeof(cases[0])), created = 0, joined = 0;
  for (unsigned i=0;i<rounds;i++) {
    fixture_sync_round(&driver,slots,ring,fixed,cases[i].operation,cases[i].failure_bits,cases[i].created);
    created += fixture_driver_created; joined += fixture_driver_joined;
    /* Reuse is caller-owned and happens only after exact native reclamation. */
    memset(&driver,0,sizeof(driver));
  }
  SYNC_CHECK(created == 8 && joined == 8);
  printf("task-driver-posix-sync-ok rounds=%u created=%u joined=%u\n",rounds,created,joined);
  return 0;
}
"#;
