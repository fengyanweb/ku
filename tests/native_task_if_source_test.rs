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
    run_case_with_workers(index, source, stdout, failure, None);
}

fn run_case_with_workers(
    index: usize,
    source: &str,
    stdout: &str,
    failure: Option<&str>,
    workers: Option<usize>,
) {
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
    let mut ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    // The CLI materializes this standard-module capability before checking.
    // Real import expansion is independently exercised by the source CLI case.
    for item in &mut ast.items {
        if let ku::ast::Item::Import(import) = item {
            if import.path == "std.time" {
                *item = ku::ast::Item::Module(ku::ast::ModuleDecl {
                    name: "std:time".into(),
                    span: import.span,
                });
            }
        }
    }
    Checker::new().check(&ast).unwrap();
    let native =
        task_lower::lower_program(&ast).unwrap_or_else(|error| panic!("case {index}: {error}"));
    let mut generated = c::generate_native_task_c_source(&native, &Default::default()).unwrap();
    if let Some(workers) = workers {
        let original = "ku_task_root_worker_count(), startup_deadline);";
        assert_eq!(generated.matches(original).count(), 1);
        generated = generated.replacen(original, &format!("{workers}u, startup_deadline);"), 1);
    }
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
fn native_task_while_finite_source_cases_match_interpreter_and_close_allocations() {
    let cases = [
        (
            r#"
async fn main(): null! {
    index = 0
    while (false) {}
    while (index < 3) { println(index) index = index + 1 }
    println(index)
    return ok(null)
}
"#,
            "0\n1\n2\n3\n",
            None,
        ),
        (
            r#"
async fn main(): null! {
    outer = 0
    while (outer < 2) {
        inner = 0
        while (inner < 2) {
            if (inner == 0) { inner = inner + 1 } else { inner = 2 }
        }
        if (true) { outer: int = outer + 10 println(outer) }
        outer = outer + 1
    }
    println(outer)
    return ok(null)
}
"#,
            "10\n11\n2\n",
            None,
        ),
        (
            r#"
async fn Text(): str! { return ok("probe") }
async fn Flag(index: int, text: str): bool! { println(text) return ok(index < 2) }
async fn main(): null! {
    index = 0
    while ((await Flag(index, (await Text())?))?) {
        text = "body"
        unused = ok(index)
        println(text)
        index = index + 1
    }
    return ok(null)
}
"#,
            "probe\nbody\nprobe\nbody\nprobe\n",
            None,
        ),
        (
            r#"
async fn Flag(index: int): bool! { println(index) return ok(true) }
async fn main(): null! {
    index = 0
    while ((index < 2) && (await Flag(index))?) { index = index + 1 }
    println(index)
    return ok(null)
}
"#,
            "0\n1\n2\n",
            None,
        ),
        (
            r#"
async fn Child(): int! { return ok(7) }
async fn main(): null! {
    while (true) { child = Child() return ok(null) }
    println("unreachable")
    return ok(null)
}
"#,
            "",
            None,
        ),
        (
            r#"
async fn Child(): int! { return ok(7) }
async fn main(): null! {
    while (true) { child = Child() println("before fail") fail "while body failed" }
    println("unreachable")
    return ok(null)
}
"#,
            "before fail\n",
            Some("while body failed"),
        ),
        (
            r#"
async fn Flag(index: int): bool! {
    println(index)
    if (index == 1) { fail "while condition failed" }
    return ok(true)
}
async fn main(): null! {
    index = 0
    while ((await Flag(index))?) { println("body") index = index + 1 }
    println("unreachable")
    return ok(null)
}
"#,
            "0\nbody\n1\n",
            Some("while condition failed"),
        ),
        (
            r#"
async fn main(): null! {
    index = 3
    while (index > 0) { println(index) index = index / 0 }
    println("unreachable")
    return ok(null)
}
"#,
            "3\n",
            Some("division by zero"),
        ),
        (
            r#"
async fn main(): null! {
    result = ok(true)
    while (result?) { println("once") return ok(null) }
    return ok(null)
}
"#,
            "once\n",
            None,
        ),
        (
            r#"
async fn Flag(): bool! { return ok(true) }
async fn main(): null! {
    pending = Flag()
    while ((await pending)?) { println("once") return ok(null) }
    return ok(null)
}
"#,
            "once\n",
            None,
        ),
        (
            r#"
async fn Probe(index: int, marker: null): bool! { return ok(index < 2) }
async fn Parent(held: str): str! {
    index = 0
    while ((await Probe(index, println(held)))?) { index = index + 1 }
    return ok(held)
}
async fn main(): null! {
    value = (await Parent("held"))?
    println(value)
    return ok(null)
}
"#,
            "held\nheld\nheld\nheld\n",
            None,
        ),
    ];
    // All programs terminate or fail after a finite number of iterations.
    // Infinite-loop cancellation belongs to the separate real host fixture.
    for (index, (source, stdout, failure)) in cases.into_iter().enumerate() {
        run_case(200 + index, source, stdout, failure);
    }
}

#[test]
fn native_task_integer_for_matches_interpreter_and_closes_allocations() {
    let cases = [
        (
            r#"
async fn Child(value: int): int! { return ok(value) }
async fn main(): null! {
    item = 90
    limit = 3
    total = 0
    for item in 0 { println(999) }
    for item in 1 { println(item) }
    for item in limit {
        limit = 0
        total = total + item
        item = 99
        println((await Child(item))?)
    }
    println(total)
    println(limit)
    println(item)
    return ok(null)
}
"#,
            "0\n99\n99\n99\n3\n0\n90\n",
            None,
        ),
        (
            "async fn main(): null! { for item in 2 { for item in 2 { println(item) } println(item) } return ok(null) }",
            "0\n1\n0\n0\n1\n1\n",
            None,
        ),
        (
            "async fn Bound(): int! { println(\"bound\") return ok(3) } async fn main(): null! { for item in (await Bound())? { println(item) } return ok(null) }",
            "bound\n0\n1\n2\n",
            None,
        ),
        (
            "async fn Pick(limit: int): int! { for item in limit { if (item == 2) { return ok(item) } } return ok(-1) } async fn main(): null! { println((await Pick(5))?) println((await Pick(0))?) return ok(null) }",
            "2\n-1\n",
            None,
        ),
        (
            "async fn Bound(): int! { return ok(-1) } async fn main(): null! { for item in (await Bound())? { println(999) } return ok(null) }",
            "",
            Some("for int iterator expects a non-negative int"),
        ),
        (
            "async fn Child(value: str): str! { return ok(value) } async fn main(): null! { for item in 3 { held = \"body\" pending = Child(\"discarded\") unused = ok(\"local\") println(held) } println(\"done\") return ok(null) }",
            "body\nbody\nbody\ndone\n",
            None,
        ),
    ];
    for (index, (source, stdout, failure)) in cases.into_iter().enumerate() {
        run_case(400 + index, source, stdout, failure);
    }
}

#[test]
fn native_task_break_continue_match_interpreter_and_close_each_exited_scope() {
    let cases = [
        (
            "async fn main(): null! { for item in 5 { if (item == 1) { continue } if (item == 3) { break } println(item) } println(\"done\") return ok(null) }",
            "0\n2\ndone\n",
        ),
        (
            "async fn Before(index: int): bool! { println(index) return ok(index < 5) } async fn main(): null! { index = 0 while ((await Before(index))?) { index = index + 1 if (index == 1) { continue } if (index == 3) { break } println(index) } println(index) return ok(null) }",
            "0\n1\n2\n2\n3\n",
        ),
        (
            "async fn main(): null! { outer = 90 total = 0 for outer in 3 { for inner in 3 { if (inner == 0) { continue } if (inner == 2) { break } total = total + outer + inner } println(outer) } println(total) println(outer) return ok(null) }",
            "0\n1\n2\n6\n90\n",
        ),
        (
            "async fn Child(value: str): str! { return ok(value) } async fn main(): null! { root = Child(\"kept\") for item in 3 { text = \"body\" pending = Child(\"discarded\") unused = ok(\"local\") if (item == 0) { continue } else { break } } println((await root)?) return ok(null) }",
            "kept\n",
        ),
        (
            "async fn main(): null! { limit = 3 for item in limit { limit = 0 println(item) item = 99 continue } println(limit) return ok(null) }",
            "0\n1\n2\n0\n",
        ),
        (
            "async fn main(): null! { for item in 3 { if (item == 0) { continue } text = \"later\" println(text) if (item == 1) { continue } break } return ok(null) }",
            "later\nlater\n",
        ),
        (
            "async fn main(): null! { while (true) { break } println(\"after\") while (false) { continue } return ok(null) }",
            "after\n",
        ),
    ];
    for (index, (source, stdout)) in cases.into_iter().enumerate() {
        run_case(500 + index, source, stdout, None);
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
            "async fn main(): null! { if (false) { for item in [[1], [2]] {} } return ok(null) }",
            "native async subset does not support",
        ),
        (
            "async fn main(): null! { try { println(1) } finally { while (false) { println(0) } } return ok(null) }",
            "native async subset does not support loops inside a native Task finally body",
        ),
        (
            "async fn main(): null! { try { println(1) } finally { for item in 0 { println(item) } } return ok(null) }",
            "native async subset does not support loops inside a native Task finally body",
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

#[test]
fn native_task_sleep_uses_driver_wait_and_keeps_single_worker_available() {
    let cases = [
        (
            "import time from \"std.time\"\nasync fn main(): null! { wait = 1 time.sleep(wait)? time.sleep(0)? println(7) return ok(null) }",
            "7\n", None,
        ),
        (
            "import time from \"std.time\"\nasync fn Bound(): int! { println(3) return ok(1) } async fn main(): null! { time.sleep((await Bound())?)? println(4) return ok(null) }",
            "3\n4\n", None,
        ),
        (
            "import time from \"std.time\"\nasync fn main(): null! { time.sleep(-1)? println(999) return ok(null) }",
            "", Some("duration must be >= 0"),
        ),
        (
            "import time from \"std.time\"\nasync fn Slow(): int! { time.sleep(250)? return ok(2) } async fn Fast(): int! { time.sleep(0)? return ok(1) } async fn main(): null! { slow = Slow() fast = Fast(); first = (await fast)? println(first) second = (await slow)? println(second) return ok(null) }",
            "1\n2\n", None,
        ),
        (
            "import time from \"std.time\"\nasync fn Child(): null! { time.sleep(250)? return ok(null) } async fn main(): null! { child = Child() time.sleep(20)? println(8) return ok(null) }",
            "8\n", None,
        ),
        (
            "import time from \"std.time\"\nasync fn main(): null! { for item in 3 { time.sleep(1)? if (item == 1) { continue } println(item) } time.sleep(-1) println(5) return ok(null) }",
            "0\n2\n5\n", None,
        ),
    ];
    for (index, (source, stdout, failure)) in cases.into_iter().enumerate() {
        run_case_with_workers(900 + index, source, stdout, failure, Some(1));
    }
}

#[test]
fn native_task_recoverable_catch_routes_errors_and_drains_owned_scopes() {
    let cases = [
        (
            r#"async fn main(): null! {
                try { fail "bad 中文😀" } catch (problem) {
                    println(problem.domain) println(problem.code)
                    println(problem.message) println(problem.message)
                }
                println(8) return ok(null)
            }"#,
            "ku\nfail\nbad 中文😀\nbad 中文😀\n8\n",
            None,
        ),
        (
            r#"async fn Bad(): str! { fail "child error" }
            async fn main(): null! {
                try { value = (await Bad())? println(value) }
                catch (problem) { println(problem.message) }
                return ok(null)
            }"#,
            "child error\n",
            None,
        ),
        (
            r#"async fn main(): null! {
                try {
                    try { fail "rethrow" } catch (inner) { fail inner }
                } catch (outer) { println(outer.message) }
                return ok(null)
            }"#,
            "rethrow\n",
            None,
        ),
        (
            r#"import time from "std.time"
            async fn main(): null! {
                count = 0
                for item in 3 {
                    try { time.sleep(-1)? count = 999 }
                    catch (problem) { count = count + 1 println(problem.code) }
                }
                println(count) return ok(null)
            }"#,
            "invalid_duration\ninvalid_duration\ninvalid_duration\n3\n",
            None,
        ),
        (
            r#"async fn main(): null! {
                try { println(2) } catch (unused) { println(999) }
                try { return ok(null) } catch (unused) { println(999) }
                println(998) return ok(null)
            }"#,
            "2\n",
            None,
        ),
        (
            r#"async fn main(): null! {
                try { fail "first" } catch (problem) { fail "second" }
                return ok(null)
            }"#,
            "",
            Some("second"),
        ),
        (
            r#"import time from "std.time"
            async fn Child(): null! { time.sleep(100)? return ok(null) }
            async fn main(): null! {
                child = Child()
                try {
                    nested = Child()
                    time.sleep(1)? fail "caught"
                } catch (problem) { println(problem.message) }
                time.sleep(1)? println(9) return ok(null)
            }"#,
            "caught\n9\n",
            None,
        ),
        (
            r#"async fn main(): null! {
                try { value = 1 / 0 println(value) }
                catch (problem) { println(999) }
                return ok(null)
            }"#,
            "",
            Some("division by zero"),
        ),
        (
            r#"import time from "std.time"
            async fn Child(): null! {
                try { time.sleep(250)? }
                catch (problem) { println(problem.message) println(999) }
                return ok(null)
            }
            async fn main(): null! {
                child = Child() time.sleep(20)? println(7) return ok(null)
            }"#,
            "7\n",
            None,
        ),
        (
            r#"async fn main(): null! {
                try { fail "preserved" } catch (problem) { fail problem }
                return ok(null)
            }"#,
            "",
            Some("preserved"),
        ),
    ];
    for (index, (source, stdout, failure)) in cases.into_iter().enumerate() {
        run_case_with_workers(1000 + index, source, stdout, failure, Some(1));
    }
}

#[test]
fn native_task_callable_values_move_clone_and_invoke_real_tasks() {
    for (index, source, output) in [
        (
            0,
            r#"async fn Add(value: int): int! { return ok(value + 1) }
async fn main(): null! {
    selected: async fn(int): int! = Add
    println((await selected(4))?)
    println((await selected(8))?)
    cloned = selected.clone()
    moved = cloned
    println((await moved(12))?)
    println((await selected.clone()(15))?)
    return ok(null)
}"#,
            "5\n9\n13\n16\n",
        ),
        (
            1,
            r#"async fn Emit(): null! { println("actual child") return ok(null) }
async fn main(): null! {
    selected = Emit;
    (await selected())?;
    (await selected())?;
    return ok(null)
}"#,
            "actual child\nactual child\n",
        ),
        (
            2,
            r#"async fn Left(): int! { println("left") return ok(2) }
async fn Right(): int! { println("right") return ok(3) }
async fn Add(left: int, right: int): int! { println("target") return ok(left + right) }
async fn main(): null! {
    selected = Add
    println((await selected((await Left())?, (await Right())?))?)
    return ok(null)
}"#,
            "left\nright\ntarget\n5\n",
        ),
        (
            3,
            r#"async fn Identity(value: str!, extra: str): str! { return value }
async fn main(): null! {
    selected = Identity
    text = "中文🙂"
    println((await selected(ok(text), "extra"))?)
    println((await selected(ok("again"), "unused"))?)
    return ok(null)
}"#,
            "中文🙂\nagain\n",
        ),
        (
            4,
            r#"async fn Broken(): int! { fail "selected failure" }
async fn main(): null! {
    selected = Broken
    try { (await selected())? } catch(error) { println(error.message) }
    try { (await selected())? } catch(error) { println(error.code) }
    return ok(null)
}"#,
            "selected failure\nfail\n",
        ),
        (
            5,
            r#"async fn Child(value: int): int! { return ok(value) }
async fn main(): null! {
    selected = Child
    if (true) { inner = selected.clone() println((await inner(7))?) }
    println((await selected(9))?)
    return ok(null)
}"#,
            "7\n9\n",
        ),
    ] {
        run_case_with_workers(1500 + index, source, output, None, Some(1));
    }
}

#[test]
fn native_task_finally_source_preserves_normal_nonlocal_and_error_routes() {
    let cases = [
        (
            r#"async fn main(): null! {
                marker = 0
                try { marker = 1 } finally { marker = marker + 10 }
                println(marker)
                try { fail "caught" } catch (error) { println(error.message) }
                finally { marker = marker + 100 }
                println(marker) return ok(null)
            }"#,
            "11\ncaught\n111\n",
            None,
        ),
        (
            r#"async fn Probe(): str! {
                text = "中文😀"
                try { return ok(text) } finally { println("finished") }
                return ok("unreachable")
            }
            async fn main(): null! { println((await Probe())?) return ok(null) }"#,
            "finished\n中文😀\n",
            None,
        ),
        (
            r#"async fn Probe(): int! {
                try { return ok(1) } finally { return ok(2) }
                return ok(3)
            }
            async fn main(): null! { println((await Probe())?) return ok(null) }"#,
            "2\n",
            None,
        ),
        (
            r#"async fn main(): null! {
                try {
                    try { fail "body" } catch (inner) { println(inner.message) }
                    finally { fail "replacement" }
                } catch (outer) { println(outer.message) }
                return ok(null)
            }"#,
            "body\nreplacement\n",
            None,
        ),
        (
            r#"async fn main(): null! {
                marker = 0
                for item in 4 {
                    try {
                        if (item == 1) { continue }
                        if (item == 3) { break }
                        marker = marker + item
                    } finally { marker = marker + 10 }
                }
                println(marker) return ok(null)
            }"#,
            "42\n",
            None,
        ),
        (
            r#"async fn main(): null! {
                try { try { fail "primary" } finally { println(1) } }
                finally { println(2) }
                return ok(null)
            }"#,
            "1\n2\n",
            Some("primary"),
        ),
        (
            r#"async fn main(): null! {
                bound = 9223372036854775807
                try { println(bound + 1) } finally { println("must skip") }
                return ok(null)
            }"#,
            "",
            Some("integer overflow"),
        ),
        (
            r#"import time from "std.time"
            async fn Probe(): int! {
                try { time.sleep(0)? return ok(7) } finally { println(9) }
                return ok(0)
            }
            async fn main(): null! {
                try { println((await Probe())?) } finally { println(10) }
                return ok(null)
            }"#,
            "9\n7\n10\n",
            None,
        ),
        (
            r#"async fn Probe(): str! {
                for item in 3 {
                    try { return ok("discarded") } finally {
                        if (item < 2) { continue }
                        return ok("replacement")
                    }
                }
                return ok("unreachable")
            }
            async fn main(): null! { println((await Probe())?) return ok(null) }"#,
            "replacement\n",
            None,
        ),
        (
            r#"async fn Probe(): int! {
                for item in 3 { try { return ok(1) } finally { break } }
                return ok(2)
            }
            async fn main(): null! { println((await Probe())?) return ok(null) }"#,
            "2\n",
            None,
        ),
        (
            r#"async fn Probe(): str! {
                try { return ok("saved") } finally {
                    try { fail "local" } catch (error) { println(error.message) }
                    println(8)
                }
                return ok("unreachable")
            }
            async fn main(): null! { println((await Probe())?) return ok(null) }"#,
            "local\n8\nsaved\n",
            None,
        ),
        (
            r#"async fn main(): null! {
                try { try { fail "body" } finally { fail "replacement" } }
                catch (error) { println(error.message) }
                return ok(null)
            }"#,
            "replacement\n",
            None,
        ),
        (
            r#"async fn main(): null! {
                for item in 3 {
                    try {
                        try { fail "body" } finally {
                            if (item < 2) { continue }
                            fail "replacement"
                        }
                    } catch (error) { println(error.message) }
                }
                return ok(null)
            }"#,
            "replacement\n",
            None,
        ),
        (
            r#"async fn main(): null! {
                for item in 3 {
                    try { try { fail "body" } finally { break } }
                    catch (error) { println(error.message) }
                }
                println("after") return ok(null)
            }"#,
            "after\n",
            None,
        ),
    ];
    for (index, (source, stdout, failure)) in cases.into_iter().enumerate() {
        run_case_with_workers(5000 + index, source, stdout, failure, Some(1));
    }
}
