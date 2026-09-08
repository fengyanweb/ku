//! Real source -> verified Task IR -> generated factories/continuations/root.
//! No fixture callback supplies Start, Await or scope cleanup on its behalf.
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use ku::{backend::c, checker::Checker, ir::task_lower, lexer::Lexer, parser::Parser};
use native_harness::{
    compile_harness, run_bounded, TempDir, BUILD_LIMITS, BUILD_TIMEOUT, RUN_LIMITS, RUN_TIMEOUT,
};
use std::{fs, process::Command};

fn checked(source: &str) -> ku::ast::Program {
    let ast = Parser::new(Lexer::new(source).lex().expect("source lexes"))
        .parse_program()
        .expect("source parses");
    Checker::new()
        .check(&ast)
        .unwrap_or_else(|error| panic!("{error}\n{source}"));
    ast
}

fn native(source: &str) -> String {
    let tasks =
        task_lower::lower_program(&checked(source)).expect("source lowers to verified Task IR");
    c::generate_native_task_c_source(&tasks, &Default::default()).expect("native Task C generation")
}

const MOVES_AND_AWAITS: &str = r#"
async fn main(): null! {
    first = Child(3)
    second = Child(4)
    moved = first
    a = (await moved)?
    b = (await second)?
    text = (await Text("Ku 世界"))?
    println(a)
    println(b)
    println(text)
    return ok(null)
}
async fn Child(value: int): int! { return ok(value) }
async fn Text(value: str): str! { return ok(value) }
"#;

#[test]
fn native_task_source_generated_start_await_and_scope_execute_without_source() {
    let cases = [
        (MOVES_AND_AWAITS, "3\n4\nKu 世界\n", "", true),
        (
            "async fn main(): null! { first = Child(1) second = Child(2) third = Child(3) return ok(null) } async fn Child(value: int): int! { return ok(value) }",
            "", "", true,
        ),
        (
            "async fn Flag(value: bool): bool! { return ok(value) } async fn Empty(): null! { return ok(null) } async fn main(): null! { flag = (await Flag(true))? empty = (await Empty())? print(flag) println(empty) return ok(null) }",
            "truenull\n", "", true,
        ),
        (
            "async fn Broken(): int! { fail \"expected failure\" } async fn main(): null! { child = Broken() result = await child println(7) return ok(null) }",
            "7\n", "", true,
        ),
        (
            "async fn Broken(): int! { fail \"expected failure\" } async fn main(): null! { child = Broken() value = (await child)? println(value) return ok(null) }",
            "", "expected failure\n", false,
        ),
        (
            "async fn Number(): int! { return ok(8) } async fn Carry(text: str, value: int): str! { println(value) return ok(text) } async fn main(): null! { text = (await Carry(\"argument before await\", (await Number())?))? println(text) return ok(null) }",
            "8\nargument before await\n", "", true,
        ),
        (
            "async fn Unwrap(value: str!): str! { return value } async fn main(): null! { wrapped = ok(\"a\0b 世界\") text = (await Unwrap(wrapped))? println(text) return ok(null) }",
            "a\0b 世界\n", "", true,
        ),
        (
            "async fn println(value: int): null! { return ok(null) } async fn main(): null! { ignored = println(7) return ok(null) }",
            "", "", true,
        ),
        (
            "async fn main(): null! { text = \"kept\" text println(text) return ok(null) }",
            "kept\n", "", true,
        ),
        (
            "async fn ok(value: int): null! { fail \"user ok failed\" } async fn main(): null! { child = ok(7) result = await child fail \"main done\" }",
            "", "main done\n", false,
        ),
    ];
    for (index, (source, expected_out, expected_err, success)) in cases.into_iter().enumerate() {
        let generated = native(source);
        for forbidden in ["run_source", "const SOURCE", "Task.new", "task.spawn"] {
            assert!(!generated.contains(forbidden));
        }
        assert!(generated.contains("ku_task_driver_wait_result("));
        assert!(generated.contains("ku_task_root_driver"));
        let directory = TempDir::new(&format!("native-task-source-{index}"));
        let path = directory.path().join("program.c");
        fs::write(&path, generated).unwrap();
        let Some(executable) = compile_harness(directory.path(), &path, "program") else {
            assert!(
                std::env::var_os("GITHUB_ACTIONS").is_none(),
                "CI must execute native source Tasks"
            );
            continue;
        };
        fs::remove_file(path).unwrap();
        let output = run_bounded(
            Command::new(executable).current_dir(directory.path()),
            RUN_TIMEOUT,
            RUN_LIMITS,
        )
        .expect("native source Tasks terminate within the bounded process watchdog");
        assert_eq!(
            output.status.success(),
            success,
            "case {index}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().replace('\r', ""),
            expected_out,
            "case {index}"
        );
        assert_eq!(
            String::from_utf8(output.stderr).unwrap().replace('\r', ""),
            expected_err,
            "case {index}"
        );
    }
}

#[test]
fn native_task_source_lowering_keeps_pending_away_from_argument_and_move_operations() {
    use ku::ir::task::{TaskOp, TaskTerminator};
    let native = task_lower::lower_program(&checked(MOVES_AND_AWAITS)).unwrap();
    let main = native
        .tasks
        .functions
        .iter()
        .find(|function| function.id == native.entry)
        .unwrap();
    let awaits = main
        .states
        .iter()
        .filter(|state| matches!(state.terminator, TaskTerminator::Await { .. }))
        .collect::<Vec<_>>();
    assert_eq!(awaits.len(), 3);
    assert!(awaits.iter().all(|state| state.operations.is_empty()));
    assert_eq!(
        main.states
            .iter()
            .flat_map(|state| &state.operations)
            .filter(|operation| matches!(operation, TaskOp::Start { .. }))
            .count(),
        3
    );
}

#[test]
fn native_task_source_exit_owns_normal_results_without_erasing_await_cleanup() {
    use ku::ir::{
        task::{self, SlotId, TaskConstant, TaskLimits, TaskOp, TaskSlotType, TaskTerminator},
        IrType,
    };

    let source = r#"
async fn ReturnText(value: str): str! { local = value return ok(local) }
async fn FailText(): str! { retained = "held" fail "expected failure" }
async fn Question(value: str!): str! { local = value? return ok(local) }
async fn ShortAnd(gate: bool): bool! { return ok(gate && (await Flag())?) }
async fn ShortOr(gate: bool): bool! { return ok(gate || (await Flag())?) }
async fn Flag(): bool! { return ok(true) }
async fn AwaitText(value: str): str! {
    retained = "held across await"
    child = ReturnText(value)
    output = (await child)?
    println(retained)
    return ok(output)
}
async fn main(): null! { return ok(null) }
"#;
    let native = task_lower::lower_program(&checked(source)).unwrap();
    let plan = task::verify_and_plan(&native.tasks, TaskLimits::default()).unwrap();
    let mut await_count = 0;
    for function in &native.tasks.functions {
        let frame = plan
            .functions
            .iter()
            .find(|frame| frame.function == function.id)
            .unwrap();
        assert!(frame.exit_bridge && frame.hosted, "{}", function.name);
        let owned: Vec<_> = function
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| {
                matches!(&slot.ty, TaskSlotType::Value { ty, borrowed: false }
                    if matches!(ty, IrType::Str | IrType::Result(_)))
            })
            .map(|(index, _)| SlotId(index))
            .collect();
        assert!(owned.iter().all(|slot| frame.slots.contains(slot)));
        let cleanup: Vec<_> = owned
            .iter()
            .rev()
            .map(|slot| TaskOp::DropIfInit { slot: *slot })
            .collect();
        let mut exit_count = 0;
        for state in &function.states {
            match state.terminator {
                TaskTerminator::Exit { value } => {
                    exit_count += 1;
                    assert_eq!(
                        function.slots[value.0].ty,
                        TaskSlotType::Value {
                            ty: function.result.clone(),
                            borrowed: false,
                        }
                    );
                    assert!(
                        state
                            .operations
                            .iter()
                            .all(|operation| { !matches!(operation, TaskOp::DropIfInit { .. }) }),
                        "{} normal Exit must leave remaining Values for handoff-first glue",
                        function.name
                    );
                }
                TaskTerminator::Complete { .. } => {
                    panic!("{} still uses legacy normal completion", function.name)
                }
                TaskTerminator::Await {
                    cleanup: cleanup_state,
                    ..
                } => {
                    await_count += 1;
                    assert!(state.operations.is_empty(), "Pending must not replay setup");
                    let block = &function.states[cleanup_state.0];
                    assert_eq!(block.terminator, TaskTerminator::Terminate);
                    // Keep every conditional Value drop in reverse declaration
                    // order, including slots only initialized on another edge.
                    // Task owners are deliberately absent: the host hands them
                    // off before entering this cancellation-only successor.
                    assert_eq!(block.operations, cleanup, "{}", function.name);
                }
                _ => {}
            }
        }
        assert!(exit_count > 0, "{} has no normal Exit", function.name);
    }
    assert_eq!(await_count, 3);

    let failed = native
        .tasks
        .functions
        .iter()
        .find(|function| function.name == "FailText")
        .unwrap();
    let state = &failed.states[failed.entry.0];
    let TaskTerminator::Exit { value } = state.terminator else {
        panic!("fail must stage a normal user Result");
    };
    assert!(state.operations.iter().any(|operation| {
        matches!(operation, TaskOp::Init {
            dst,
            value: TaskConstant::Err { domain, code, message, .. },
        } if *dst == value && domain == "ku" && code == "fail" && message == "expected failure")
    }));

    let question = native
        .tasks
        .functions
        .iter()
        .find(|function| function.name == "Question")
        .unwrap();
    let TaskTerminator::TryResult {
        ok_value,
        err_result,
        ok,
        err,
        ..
    } = question.states[question.entry.0].terminator
    else {
        panic!("? must retain both typed Result successors");
    };
    assert_eq!(
        question.states[err.0].terminator,
        TaskTerminator::Exit { value: err_result }
    );
    let success = &question.states[ok.0];
    assert!(matches!(success.terminator, TaskTerminator::Exit { .. }));
    assert!(success
        .operations
        .iter()
        .any(|operation| matches!(operation, TaskOp::Move { src, .. } if *src == ok_value)));

    for (name, right_on_true) in [("ShortAnd", true), ("ShortOr", false)] {
        let function = native
            .tasks
            .functions
            .iter()
            .find(|function| function.name == name)
            .unwrap();
        let TaskTerminator::Branch {
            then_state,
            else_state,
            ..
        } = function.states[function.entry.0].terminator
        else {
            panic!("{name} lost short-circuit control flow");
        };
        let (right, join) = if right_on_true {
            (then_state, else_state)
        } else {
            (else_state, then_state)
        };
        let skipped = &function.states[join.0];
        assert!(matches!(skipped.terminator, TaskTerminator::Exit { .. }));
        assert!(skipped
            .operations
            .iter()
            .all(|operation| !matches!(operation, TaskOp::Start { .. })));
        assert_eq!(
            function.states[right.0]
                .operations
                .iter()
                .filter(|operation| matches!(operation, TaskOp::Start { .. }))
                .count(),
            1
        );
        let TaskTerminator::Jump { target: poll } = function.states[right.0].terminator else {
            panic!("{name} RHS must set up its child before a separate Await");
        };
        let TaskTerminator::Await { ready, .. } = function.states[poll.0].terminator else {
            panic!("{name} RHS lost Await");
        };
        let TaskTerminator::TryResult {
            ok,
            err,
            err_result,
            ..
        } = function.states[ready.0].terminator
        else {
            panic!("{name} RHS must propagate the awaited Result");
        };
        assert_eq!(
            function.states[err.0].terminator,
            TaskTerminator::Exit { value: err_result }
        );
        assert_eq!(
            function.states[ok.0].terminator,
            TaskTerminator::Jump { target: join }
        );
        assert!(function.states[ok.0]
            .operations
            .iter()
            .any(|operation| matches!(operation, TaskOp::Copy { .. })));
    }
}

#[test]
fn native_task_source_zero_task_exit_persists_owned_locals_not_dead_copy_temporaries() {
    use ku::ir::{
        task::{self, SlotId, TaskLimits, TaskOp, TaskSlotType, TaskTerminator},
        IrType,
    };

    // A legitimate typed host may supply Owned storage for value. This source
    // only moves it to a local; it does not introduce source heap-allocation syntax.
    let source = r#"
async fn Local(value: str): str! {
    local = value
    number = 7
    println(number)
    return ok(local)
}
async fn main(): null! { return ok(null) }
"#;
    let native = task_lower::lower_program(&checked(source)).unwrap();
    let plan = task::verify_and_plan(&native.tasks, TaskLimits::default()).unwrap();
    let function = &native.tasks.functions[0];
    assert_eq!(function.name, "Local");
    let frame = plan
        .functions
        .iter()
        .find(|frame| frame.function == function.id)
        .unwrap();
    assert!(frame.exit_bridge && frame.hosted);
    assert_eq!(frame.scope_task_mask, 0);
    assert!(frame.suspensions.is_empty());
    let state = &function.states[function.entry.0];
    assert!(matches!(state.terminator, TaskTerminator::Exit { .. }));
    let moved_local = state
        .operations
        .iter()
        .find_map(|operation| match operation {
            TaskOp::Move { dst, src } if function.parameters.contains(src) => Some(*dst),
            _ => None,
        })
        .expect("source binding must move the owned parameter to a distinct local");
    assert!(!function.parameters.contains(&moved_local));
    assert!(frame.slots.contains(&moved_local));
    let mut dead_copy = 0;
    let mut dead_null = 0;
    for (index, slot) in function.slots.iter().enumerate() {
        let id = SlotId(index);
        match &slot.ty {
            TaskSlotType::Value {
                ty: IrType::Int,
                borrowed: false,
            } if !function.parameters.contains(&id) => {
                dead_copy += 1;
                assert!(
                    !frame.slots.contains(&id),
                    "dead Copy slot {index} was spilled"
                );
            }
            TaskSlotType::Value {
                ty: IrType::Null,
                borrowed: false,
            } => {
                dead_null += 1;
                assert!(
                    !frame.slots.contains(&id),
                    "unused print result {index} was spilled"
                );
                assert!(state.operations.iter().any(|operation| {
                    matches!(operation, TaskOp::Init { dst, value: task::TaskConstant::Null } if *dst == id)
                }));
            }
            TaskSlotType::Value {
                ty: IrType::Str | IrType::Result(_),
                borrowed: false,
            } => assert!(
                frame.slots.contains(&id),
                "Owned slot {index} was not persisted"
            ),
            other => panic!("unexpected zero-Task fixture slot: {other:?}"),
        }
    }
    assert_eq!(
        dead_copy, 2,
        "literal and local Copy snapshots must both exist"
    );
    assert_eq!(dead_null, 1, "println has one unused null result");
}

#[test]
fn native_task_source_subset_rejects_unimplemented_constructs() {
    let sources = [
        "async fn main(): null! { while (false) {} return ok(null) }",
        "async fn main(): null! { try { println(1) } finally { println(2) } return ok(null) }",
        "fn Sync(): int { return 1 } async fn main(): null! { return ok(null) }",
        "async fn main(): null! { value = \"first\" value = \"second\" return ok(null) }",
        "async fn Child(): int! { return ok(1) } async fn main(): null! { Child() return ok(null) }",
        "async fn main(): int! { return ok(1) }",
        "async fn Child(): int! { value = (await Child())? return ok(value) } async fn main(): null! { return ok(null) }",
    ];
    for source in sources {
        let error = task_lower::lower_program(&checked(source))
            .expect_err("unimplemented source must be rejected");
        assert!(
            error.message.contains("not support") || error.message.contains("recursive"),
            "{error}"
        );
    }
}

#[test]
fn native_task_source_ordinary_synchronous_artifacts_remain_runtime_free() {
    let ast = checked("fn main() { println(7) }");
    assert!(!task_lower::has_async_entry(&ast));
    let generated = c::generate_c_source(&ku::ir::lower_program(&ast).unwrap()).unwrap();
    for forbidden in [
        "KuTaskValue",
        "ku_task_driver_",
        "ku_task_root_",
        "_beginthreadex",
        "pthread_create",
    ] {
        assert!(
            !generated.contains(forbidden),
            "synchronous source gained Task runtime: {forbidden}"
        );
    }
}

#[test]
fn native_task_source_cli_imports_build_and_run_after_source_graph_moves() {
    for emit_only in [true, false] {
        let directory = TempDir::new("native-task-cli-import");
        let sources = directory.path().join("sources");
        fs::create_dir(&sources).unwrap();
        fs::write(
            sources.join("worker.ku"),
            "async fn Text(value: str): str! { return ok(value) }",
        )
        .unwrap();
        let main = sources.join("main.ku");
        fs::write(&main, "import { Text } from \"./worker.ku\"\nasync fn main(): null! { result = (await Text(\"import 世界\"))? println(result) return ok(null) }").unwrap();
        let binary = directory
            .path()
            .join(if cfg!(windows) { "app.exe" } else { "app" });
        let mut command = Command::new(env!("CARGO_BIN_EXE_ku"));
        command.current_dir(directory.path()).arg("build");
        if emit_only {
            command.arg("--native").arg(&main);
        } else {
            command
                .args(["--backend", "c"])
                .arg(&main)
                .arg("-o")
                .arg(&binary);
        }
        let built = run_bounded(&mut command, BUILD_TIMEOUT, BUILD_LIMITS).unwrap();
        let diagnostics = format!(
            "{}{}",
            String::from_utf8_lossy(&built.stdout),
            String::from_utf8_lossy(&built.stderr)
        );
        assert!(
            built.status.success() || (!emit_only && diagnostics.contains("C compiler not found")),
            "{diagnostics}"
        );
        let artifact = if emit_only {
            main.with_extension("c")
        } else {
            let root = sources.join(".ku/build/debug/c");
            let artifacts: Vec<_> = fs::read_dir(&root)
                .unwrap()
                .take(257)
                .map(|entry| entry.unwrap().path().join("app.c"))
                .filter(|path| path.is_file())
                .collect();
            assert_eq!(
                artifacts.len(),
                1,
                "expected one output-digest C artifact under {}",
                root.display()
            );
            artifacts[0].clone()
        };
        let generated = fs::read_to_string(&artifact)
            .unwrap_or_else(|error| panic!("{}: {error}\n{diagnostics}", artifact.display()));
        assert!(
            generated.contains("ku_task_root_driver") && generated.contains("ku_task_host_await")
        );
        assert!(!generated.contains("run_source") && !generated.contains("const SOURCE"));
        let detached = directory.path().join("detached.c");
        fs::write(&detached, generated).unwrap();
        fs::rename(&sources, directory.path().join("moved-sources")).unwrap();
        assert!(!sources.exists());
        let executable = if emit_only {
            compile_harness(directory.path(), &detached, "app")
        } else if built.status.success() {
            Some(binary)
        } else {
            None
        };
        let Some(executable) = executable else {
            assert!(
                std::env::var_os("GITHUB_ACTIONS").is_none(),
                "CI must link and run native Task CLI artifacts"
            );
            continue;
        };
        fs::remove_file(detached).unwrap();
        let output = run_bounded(
            Command::new(executable).current_dir(directory.path()),
            RUN_TIMEOUT,
            RUN_LIMITS,
        )
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().replace('\r', ""),
            "import 世界\n"
        );
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn native_task_source_cli_rejects_unsupported_and_moved_values_before_artifacts() {
    let sources = [
        "async fn main(): null! { while (false) {} return ok(null) }",
        "async fn Child(): str! { return ok(\"child\") } async fn main(): null! { child = Child() moved = child value = await child return ok(null) }",
        "async fn Child(value: str): str! { return ok(value) } async fn main(): null! { value = \"owned\" child = Child(value) println(value) return ok(null) }",
    ];
    for source in sources {
        let directory = TempDir::new("native-task-cli-reject");
        let main = directory.path().join("main.ku");
        fs::write(&main, source).unwrap();
        let error = ku::cli::run_cli(vec![
            "ku".into(),
            "build".into(),
            "--native".into(),
            main.to_string_lossy().into_owned(),
        ])
        .expect_err("unsupported or invalid ownership must not produce native code");
        assert!(
            error.message.contains("not support")
                || error.message.contains("moved")
                || error.message.contains("has already been awaited"),
            "{error}"
        );
        assert!(!main.with_extension("c").exists());
    }
}

#[test]
fn native_task_source_output_failure_unwinds_await_without_question_mark() {
    let generated = native("async fn Write(): int! { print(3) return ok(1) } async fn main(): null! { result = await Write() println(7) return ok(null) }");
    let anchor = "typedef struct KuString {";
    assert_eq!(generated.matches(anchor).count(), 1);
    // A real generated Print calls the standard flush and then receives a
    // deterministic I/O failure. No replacement Start/Await/cleanup callback.
    let source = generated.replacen(
        anchor,
        &format!(
            r#"
static int ku_fixture_flush(FILE* stream) {{
  int status=fflush(stream);
  return stream==stdout ? EOF : status;
}}
#define fflush ku_fixture_flush
{anchor}"#
        ),
        1,
    );
    let directory = TempDir::new("native-task-output-failure");
    let path = directory.path().join("program.c");
    fs::write(&path, source).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must run native Task output failure"
        );
        return;
    };
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .unwrap();
    assert!(!output.status.success());
    assert_eq!(output.stdout, b"3");
    assert_eq!(
        String::from_utf8(output.stderr).unwrap().replace('\r', ""),
        "output write failed\n"
    );
}

#[test]
fn native_task_source_diagnostic_write_failure_still_destroys_runtime() {
    let generated = native("async fn Child(): int! { return ok(1) } async fn main(): null! { child = Child() fail \"expected failure\" }");
    let anchor = "typedef struct KuString {";
    assert_eq!(generated.matches(anchor).count(), 1);
    let source = generated.replacen(
        anchor,
        &format!(
            r#"
static size_t ku_fixture_write(const void* data, size_t size, size_t count, FILE* stream) {{
  return stream==stderr ? 0 : fwrite(data,size,count,stream);
}}
#define fwrite ku_fixture_write
{anchor}"#
        ),
        1,
    );
    let done = "  return exit_code;\n}\n";
    assert_eq!(source.matches(done).count(), 1);
    let source = source.replacen(
        done,
        "  fputs(\"runtime destroyed\\n\",stdout);\n  return exit_code;\n}\n",
        1,
    );
    let directory = TempDir::new("native-task-diagnostic-failure");
    let path = directory.path().join("program.c");
    fs::write(&path, source).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must run native Task diagnostic failure"
        );
        return;
    };
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "runtime destroyed\n"
    );
    assert_eq!(
        String::from_utf8(output.stderr).unwrap().replace('\r', ""),
        "\n"
    );
}
