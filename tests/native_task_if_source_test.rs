//! Real source If -> checked scoped Task IR -> native root, no replacement
//! Start/Await/cleanup/clock callbacks. These cases use static string literals;
//! the ledger covers runtime instances, not dynamic string ownership injection.
#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;
#[path = "support/native_task_ledger.rs"]
mod native_task_ledger;

use ku::{backend::c, checker::Checker, ir::task_lower, lexer::Lexer, parser::Parser};
use native_harness::{
    compile_harness, run_bounded, TempDir, NATIVE_THREAD_LIFECYCLE_HARNESS, RUN_LIMITS, RUN_TIMEOUT,
};
use std::{fs, path::Path, process::Command};

fn normalized(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec())
        .expect("UTF-8 output")
        .replace('\r', "")
}

fn run_case(index: usize, source: &str, stdout: &str, failure: Option<&str>) {
    let directory = TempDir::new(&format!("native-task-if-source-{index}"));
    let ku_path = directory.path().join("program.ku");
    fs::write(&ku_path, source).unwrap();
    let interpreted = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_ku"))
            .current_dir(directory.path())
            .arg("run")
            .arg(&ku_path),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("bounded interpreter comparison");
    let expected_code = if failure.is_some() { 1 } else { 0 };
    assert_eq!(
        interpreted.status.code(),
        Some(expected_code),
        "case {index}: {}",
        normalized(&interpreted.stderr)
    );
    assert_eq!(
        normalized(&interpreted.stdout),
        stdout,
        "case {index}: interpreter stdout"
    );
    if let Some(message) = failure {
        assert!(
            normalized(&interpreted.stderr).contains(message),
            "case {index}: {}",
            normalized(&interpreted.stderr)
        );
    } else {
        assert!(
            interpreted.stderr.is_empty(),
            "case {index}: {}",
            normalized(&interpreted.stderr)
        );
    }
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let native = task_lower::lower_program(&ast).unwrap();
    let generated = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
    for forbidden in ["run_source", "const SOURCE", "Task.new", "task.spawn"] {
        assert!(!generated.contains(forbidden));
    }
    for anchor in ["typedef struct KuString {", "int main(void) {"] {
        assert_eq!(generated.matches(anchor).count(), 1);
    }
    let instrumentation = format!(
        "{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{}\n{}\n{}\n",
        native_task_ledger::LEDGER_LOCK,
        native_allocation_harness::ALLOCATION_HOOK,
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
            "static int fixture_real_root(void) {",
            1,
        );
    generated.push_str(
        r#"
int main(void) {
  int status=fixture_real_root();
  FixtureLedger ledger=fixture_ledger();
  CHECK(ledger.calls && !ledger.allocations && !ledger.bytes && !ledger.overflow);
  return status;
}
"#,
    );
    let c_path = directory.path().join("program.c");
    fs::write(&c_path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &c_path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute source If native binaries"
        );
        eprintln!("case {index}: no C compiler; artifact and interpreter checks ran, native execution skipped");
        return;
    };
    fs::remove_file(&c_path).unwrap();
    fs::remove_file(&ku_path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("bounded native source If");
    assert_eq!(
        output.status.code(),
        Some(expected_code),
        "case {index}: {}",
        normalized(&output.stderr)
    );
    assert_eq!(
        normalized(&output.stdout),
        stdout,
        "case {index}: native stdout"
    );
    assert_eq!(normalized(&output.stdout), normalized(&interpreted.stdout));
    assert_eq!(
        normalized(&output.stderr),
        failure
            .map(|message| format!("{message}\n"))
            .unwrap_or_default()
    );
}

#[test]
fn native_task_copy_assignment_source_values_branches_short_circuit_and_failures() {
    let cases = [
        (
            r#"
async fn main(): null! {
    number = 1 flag = true unit = null
    number = number flag = flag unit = unit
    number = 7 flag = false unit = null
    println(number) println(flag) println(unit)
    return ok(null)
}
"#,
            "7\nfalse\nnull\n",
            None,
        ),
        (
            r#"
async fn Right(value: int): int! { println(value) return ok(5) }
async fn main(): null! {
    value = 37
    value = value + (await Right(value))?
    println(value)
    return ok(null)
}
"#,
            "37\n42\n",
            None,
        ),
        (
            r#"
async fn Probe(gate: bool): int! {
    value = 10
    if (gate) { value = value + 1 } else { value = value + 2 }
    if (gate) { value: int = value + 100 value = value + 1 println(value) }
    return ok(value)
}
async fn main(): null! {
    first = (await Probe(true))? println(first)
    second = (await Probe(false))? println(second)
    return ok(null)
}
"#,
            "112\n11\n12\n",
            None,
        ),
        (
            r#"
async fn Unexpected(): bool! { println("unexpected") fail "unexpected RHS" }
async fn main(): null! {
    flag = true
    flag = flag || (await Unexpected())?
    println(flag)
    flag = flag && false
    flag = flag && (await Unexpected())?
    println(flag)
    return ok(null)
}
"#,
            "true\nfalse\n",
            None,
        ),
        (
            r#"
async fn Broken(value: int): int! { println(value) fail "assignment rhs failed" }
async fn main(): null! {
    value = 37 println(value)
    value = value + (await Broken(value))?
    println(value)
    return ok(null)
}
"#,
            "37\n37\n",
            Some("assignment rhs failed"),
        ),
        (
            r#"
async fn main(): null! {
    value = 37 println(value)
    value = value / 0
    println(value)
    return ok(null)
}
"#,
            "37\n",
            Some("division by zero"),
        ),
    ];
    for (index, (source, stdout, failure)) in cases.into_iter().enumerate() {
        run_case(100 + index, source, stdout, failure);
    }
}

#[test]
fn native_task_if_real_source_branches_match_interpreter_and_close_runtime_allocations() {
    let cases = [
        (
            r#"
async fn Child(value: int): int! { return ok(value) }
async fn Grand(gate: bool): null! {
    if (true) {
        grand = Child(17)
        if (true) {
            if (gate) { value = (await grand)? println(value) }
            else { value = (await grand)? println(value + 1) }
        }
        println(19)
    }
    return ok(null)
}
async fn main(): null! {
    first = (await Grand(true))?
    second = (await Grand(false))?
    return ok(null)
}
"#,
            "17\n19\n18\n19\n",
            None,
        ),
        (
            r#"
async fn NestedText(gate: bool, value: str): str! {
    if (true) {
        held = value
        if (true) {
            if (gate) { return ok(held) }
            println(held)
        }
        return ok(held)
    }
    return ok(value)
}
async fn main(): null! {
    first = (await NestedText(true, "first"))?
    second = (await NestedText(false, "second"))?
    println(first)
    println(second)
    return ok(null)
}
"#,
            "second\nfirst\nsecond\n",
            None,
        ),
        (
            r#"
async fn Child(value: int): int! { return ok(value) }
async fn Both(gate: bool): null! {
    outer = Child(11)
    if (gate) { value = (await outer)? println(value + 1) }
    else { value = (await outer)? println(value + 2) }
    println("joined")
    return ok(null)
}
async fn main(): null! {
    first = (await Both(true))?
    second = (await Both(false))?
    return ok(null)
}
"#,
            "12\njoined\n13\njoined\n",
            None,
        ),
        (
            r#"
async fn Child(value: int): int! { return ok(value) }
async fn Terminal(gate: bool): null! {
    outer = Child(21)
    if (gate) { value = (await outer)? println(value) return ok(null) }
    value = (await outer)?
    println(value + 1)
    return ok(null)
}
async fn main(): null! {
    first = (await Terminal(true))?
    second = (await Terminal(false))?
    return ok(null)
}
"#,
            "21\n22\n",
            None,
        ),
        (
            r#"
async fn Child(value: int): int! { return ok(value) }
async fn Shadow(gate: bool): null! {
    value: int = 4
    if (gate) {
        value: int = value + 1
        println(value)
        Child: int = 99
        println(Child)
    }
    result = (await Child(value))?
    println(result)
    return ok(null)
}
async fn main(): null! {
    first = (await Shadow(true))?
    second = (await Shadow(false))?
    return ok(null)
}
"#,
            "5\n99\n4\n4\n",
            None,
        ),
        (
            r#"
async fn Select(gate: bool): null! {
    if (gate) { println(1) } else { println(2) }
    println(3)
    return ok(null)
}
async fn main(): null! {
    first = (await Select(true))?
    second = (await Select(false))?
    return ok(null)
}
"#,
            "1\n3\n2\n3\n",
            None,
        ),
        (
            r#"
async fn Unexpected(): int! { println("unexpected") return ok(1) }
async fn main(): null! {
    if (false) { child = Unexpected() }
    println("kept")
    return ok(null)
}
"#,
            "kept\n",
            None,
        ),
        (
            r#"
async fn Choose(gate: bool): null! {
    value: int = 1
    if (gate) {
        value: int = 2
        if (false) { println(0) } else { println(value) }
    } else if (true) { value: int = 3 println(value) }
    println(value)
    return ok(null)
}
async fn main(): null! {
    first = (await Choose(true))?
    second = (await Choose(false))?
    return ok(null)
}
"#,
            "2\n1\n3\n1\n",
            None,
        ),
        (
            r#"
async fn Text(gate: bool, value: str): str! {
    if (gate) { moved = value return ok(moved) } else { println(value) }
    return ok(value)
}
async fn main(): null! {
    first = (await Text(true, "a"))?
    second = (await Text(false, "b"))?
    println(first)
    println(second)
    return ok(null)
}
"#,
            "b\na\nb\n",
            None,
        ),
        (
            r#"
async fn Flag(value: bool): bool! { println(value) return ok(value) }
async fn main(): null! {
    if ((await Flag(true))? && (await Flag(false))?) { println("then") } else { println("else") }
    return ok(null)
}
"#,
            "true\nfalse\nelse\n",
            None,
        ),
        (
            r#"
async fn Child(value: int): int! { return ok(value) }
async fn main(): null! {
    outer = Child(7)
    retained = Child(8)
    if (true) { value = (await outer)? inner = Child(value) println(value) }
    println(9)
    return ok(null)
}
"#,
            "7\n9\n",
            None,
        ),
        (
            r#"
async fn Broken(): int! { fail "arm failure" }
async fn main(): null! {
    if (true) { println("before") value = (await Broken())? println(value) }
    println("after")
    return ok(null)
}
"#,
            "before\n",
            Some("arm failure"),
        ),
        (
            r#"
async fn Broken(): bool! { fail "condition failure" }
async fn main(): null! {
    if ((await Broken())?) { println("then") } else { println("else") }
    return ok(null)
}
"#,
            "",
            Some("condition failure"),
        ),
        (
            // Rust embeds a NUL byte here; Ku deliberately has no `\0` escape.
            "
async fn Text(gate: bool): str! {
    if (gate) { return ok(\"a\0b 世界\") } else { return ok(\"other\") }
}
async fn main(): null! { value = (await Text(true))? println(value) return ok(null) }
",
            "a\0b 世界\n",
            None,
        ),
    ];
    for (index, (source, stdout, failure)) in cases.into_iter().enumerate() {
        run_case(index, source, stdout, failure);
    }
}
fn assert_no_native_artifacts(root: &Path, binary: &Path) {
    assert!(
        !binary.exists(),
        "rejected source produced {}",
        binary.display()
    );
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    let mut entries = 0usize;
    while let Some((directory, depth)) = pending.pop() {
        for entry in fs::read_dir(&directory).unwrap() {
            entries += 1;
            assert!(entries <= 256, "unexpected unbounded build output tree");
            let entry = entry.unwrap();
            let ty = entry.file_type().unwrap();
            assert!(
                !ty.is_symlink(),
                "isolated rejection fixture must not follow symlinks"
            );
            let path = entry.path();
            if ty.is_dir() {
                assert!(depth < 8, "unexpected nested build output tree");
                pending.push((path, depth + 1));
            } else {
                let extension = path
                    .extension()
                    .and_then(|value| value.to_str())
                    .unwrap_or("");
                assert!(
                    !["c", "exe", "o", "obj"]
                        .iter()
                        .any(|candidate| extension.eq_ignore_ascii_case(candidate)),
                    "rejected source left native artifact {}",
                    path.display()
                );
            }
        }
    }
}

#[test]
fn native_task_if_cli_rejects_unsupported_and_ownership_before_either_artifact_path() {
    let cases = [
        (
            "async fn main(): null! { value = \"first\" value = \"second\" return ok(null) }",
            "native async subset does not support reassignment of Owned or Task values",
        ),
        (
            "async fn Child(): int! { return ok(1) } async fn main(): null! { child = Child() child = Child() return ok(null) }",
            "native async subset does not support reassignment of Owned or Task values",
        ),
        (
            "async fn main(): null! { LIMIT = 1 LIMIT = 2 return ok(null) }",
            "cannot assign to immutable variable 'LIMIT'",
        ),
        (
            "async fn main(): null! { value: int = 1 value: int = 2 return ok(null) }",
            "variable 'value' is already defined in this scope",
        ),
        (
            "async fn Child(): int! { return ok(17) } async fn Grand(gate: bool): null! { if (true) { grand = Child() if (true) { if (gate) { value = (await grand)? println(value) } else { value = (await grand)? println(value) } } else { value = (await grand)? println(value) } again = (await grand)? println(again) } return ok(null) } async fn main(): null! { return ok(null) }",
            "task 'grand' has already been awaited",
        ),
        (
            "async fn Child(): int! { return ok(17) } async fn Grand(gate: bool): null! { if (true) { grand = Child() if (true) { if (gate) { value = (await grand)? println(value) } else { println(0) } } else { value = (await grand)? println(value) } again = (await grand)? println(again) } return ok(null) } async fn main(): null! { return ok(null) }",
            "task 'grand' has already been awaited",
        ),
        (
            "async fn main(): null! { if (false) { while (false) {} } return ok(null) }",
            "native async subset does not support this statement",
        ),
        (
            "async fn Child(): int! { return ok(1) } async fn main(): null! { outer = Child() if (true) { inner = outer } return ok(null) }",
            "native async subset does not support moving a Task across lexical scopes",
        ),
        (
            "async fn main(): null! { outer = \"value\" if (true) { moved = outer } println(outer) return ok(null) }",
            "use of moved value 'outer'",
        ),
    ];
    for (index, (source, expected_error)) in cases.into_iter().enumerate() {
        for emit_only in [true, false] {
            let directory = TempDir::new(&format!("native-task-if-reject-{index}-{emit_only}"));
            let main = directory.path().join("main.ku");
            let binary = directory.path().join(if cfg!(windows) {
                "rejected.exe"
            } else {
                "rejected"
            });
            fs::write(&main, source).unwrap();
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
            let output = run_bounded(&mut command, RUN_TIMEOUT, RUN_LIMITS)
                .expect("unsupported source must reject before invoking a C compiler");
            let diagnostics = format!(
                "{}{}",
                normalized(&output.stdout),
                normalized(&output.stderr)
            );
            assert!(
                !output.status.success(),
                "rejected source succeeded: {diagnostics}"
            );
            assert!(
                diagnostics.contains(expected_error),
                "wrong rejection: {diagnostics}"
            );
            assert!(
                !diagnostics.contains("native c ok:"),
                "artifact success was announced"
            );
            assert!(
                !main.with_extension("c").exists(),
                "--native output escaped rejection"
            );
            // Includes --backend c's output-digest subtree, not only main.c.
            assert_no_native_artifacts(directory.path(), &binary);
        }
    }
}
