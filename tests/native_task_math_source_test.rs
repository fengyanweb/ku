//! End-to-end checked source -> Task IR -> native root arithmetic.
//! The interpreter is a separate bounded CLI process, not an in-process oracle.
//! No fixture replaces a helper, frame, Start/Await, or root cleanup operation.
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use ku::{backend::c, checker::Checker, ir::task_lower, lexer::Lexer, parser::Parser};
use native_harness::{compile_harness, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};
use std::{fs, process::Command};

fn normalized(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec())
        .expect("test output is UTF-8")
        .replace('\r', "")
}

fn compare_case(label: &str, source: &str, stdout: &str, failure: Option<&str>) {
    let directory = TempDir::new(&format!("task-math-{label}"));
    let ku_path = directory.path().join("program.ku");
    fs::write(&ku_path, source).expect("write interpreter source");

    let mut interpreter = Command::new(env!("CARGO_BIN_EXE_ku"));
    interpreter
        .current_dir(directory.path())
        .arg("run")
        .arg(&ku_path);
    let interpreted = run_bounded(&mut interpreter, RUN_TIMEOUT, RUN_LIMITS)
        .unwrap_or_else(|error| panic!("{label}: bounded interpreter failed: {error}"));
    let expected_code = if failure.is_some() { 1 } else { 0 };
    assert_eq!(
        interpreted.status.code(),
        Some(expected_code),
        "{label}: interpreter must exit normally, not signal or Rust panic:\n{}",
        normalized(&interpreted.stderr),
    );
    assert_eq!(
        normalized(&interpreted.stdout),
        stdout,
        "{label}: interpreter stdout"
    );
    let interpreter_error = normalized(&interpreted.stderr);
    if let Some(message) = failure {
        // E0001 is the CLI diagnostic ID, not KuError.domain/code. The CLI
        // wraps the original runtime message with its source location; this
        // comparison does not pretend to inspect the hidden Error fields.
        let heading = format!("error[E0001]: error: {message}");
        assert_eq!(
            interpreter_error.lines().next(),
            Some(heading.as_str()),
            "{label}: interpreter must preserve the checked arithmetic error",
        );
    } else {
        assert!(interpreter_error.is_empty(), "{label}: {interpreter_error}");
    }

    let ast = Parser::new(Lexer::new(source).lex().expect("math source lexes"))
        .parse_program()
        .expect("math source parses");
    Checker::new()
        .check(&ast)
        .unwrap_or_else(|error| panic!("{label}: source type check: {error}"));
    let tasks = task_lower::lower_program(&ast)
        .unwrap_or_else(|error| panic!("{label}: verified Task lowering: {error}"));
    let generated = c::generate_native_task_c_source(&tasks, &Default::default())
        .unwrap_or_else(|error| panic!("{label}: native generation: {error}"));
    assert!(!generated.contains("run_source") && !generated.contains("const SOURCE"));
    assert!(generated.contains("ku_task_root_driver"));

    let c_path = directory.path().join("program.c");
    fs::write(&c_path, generated).expect("write generated C");
    let Some(executable) = compile_harness(directory.path(), &c_path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute the real native arithmetic gate",
        );
        eprintln!("{label}: no C compiler; interpreter and generated-artifact gates ran, native execution skipped");
        return;
    };
    fs::remove_file(&c_path).expect("remove C before native execution");
    fs::remove_file(&ku_path).expect("remove Ku source before native execution");
    assert!(!c_path.exists() && !ku_path.exists());

    let native = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .unwrap_or_else(|error| panic!("{label}: bounded native process failed: {error}"));
    assert_eq!(
        native.status.code(),
        Some(expected_code),
        "{label}: native root must exit normally, not signal or compiler/sanitizer trap:\n{}",
        normalized(&native.stderr),
    );
    assert_eq!(normalized(&native.stdout), stdout, "{label}: native stdout");
    assert_eq!(
        normalized(&native.stdout),
        normalized(&interpreted.stdout),
        "{label}: interpreter/native output differs",
    );
    assert_eq!(
        normalized(&native.stderr), failure.map(|message| format!("{message}\n")).unwrap_or_default(),
        "{label}: native failure must use the original message, not a driver fault, signal, or cleanup timeout",
    );
}

// Every tested operation executes inside Calc on parameters, not only a
// compile-time literal expression. Negative quotient/remainder use truncation
// toward zero, including a negative numerator and a negative denominator.
const NORMAL_ARITHMETIC: &str = r#"
async fn Calc(a: int, b: int): null! {
    println(-a)
    println(a + b)
    println(a - b)
    println(a * b)
    println(a / b)
    println(a % b)
    return ok(null)
}
async fn main(): null! {
    first = (await Calc(13, 5))?
    second = (await Calc(-13, 5))?
    third = (await Calc(13, -5))?
    return ok(null)
}
"#;

const NORMAL_COMPARISONS: &str = r#"
async fn Ints(a: int, b: int): null! {
    println(a == b)
    println(a != b)
    println(a < b)
    println(a <= b)
    println(a > b)
    println(a >= b)
    return ok(null)
}
async fn IntegerCases(): null! {
    less = (await Ints(3, 4))?
    equal = (await Ints(4, 4))?
    adjacent = (await Ints(9223372036854775807, 9223372036854775807 - 1))?
    extremes = (await Ints((-9223372036854775807 - 1), 9223372036854775807))?
    return ok(null)
}
async fn Bools(a: bool, b: bool): null! {
    println(!a)
    println(!b)
    println(a == b)
    println(a != b)
    return ok(null)
}
async fn main(): null! {
    integers = (await IntegerCases())?
    different = (await Bools(true, false))?
    equal = (await Bools(false, false))?
    return ok(null)
}
"#;

#[test]
fn native_task_math_source_nonliteral_values_match_bounded_interpreter() {
    compare_case(
        "normal-arithmetic",
        NORMAL_ARITHMETIC,
        "-13\n18\n8\n65\n2\n3\n13\n-8\n-18\n-65\n-2\n-3\n-13\n8\n18\n-65\n-2\n3\n",
        None,
    );
    compare_case(
        "normal-comparisons",
        NORMAL_COMPARISONS,
        concat!(
            "false\ntrue\ntrue\ntrue\nfalse\nfalse\n",
            "true\nfalse\nfalse\ntrue\nfalse\ntrue\n",
            "false\ntrue\nfalse\nfalse\ntrue\ntrue\n",
            "false\ntrue\ntrue\ntrue\nfalse\nfalse\n",
            "false\ntrue\nfalse\ntrue\n",
            "true\ntrue\ntrue\nfalse\n",
        ),
        None,
    );
}

#[test]
fn native_task_math_source_failures_cannot_be_received_as_ordinary_result() {
    // MIN itself is built using the existing accepted source spelling; a
    // rejected positive 9223372036854775808 token is not an arithmetic test.
    const MIN: &str = "(-9223372036854775807 - 1)";
    const MAX: &str = "9223372036854775807";
    let cases = [
        ("neg-min", "-a", MIN, "0", "integer overflow"),
        ("add-max-one", "a + b", MAX, "1", "integer overflow"),
        ("add-min-minus-one", "a + b", MIN, "-1", "integer overflow"),
        ("sub-min-one", "a - b", MIN, "1", "integer overflow"),
        ("sub-max-minus-one", "a - b", MAX, "-1", "integer overflow"),
        ("mul-max-two", "a * b", MAX, "2", "integer overflow"),
        ("mul-min-minus-one", "a * b", MIN, "-1", "integer overflow"),
        ("divide-zero", "a / b", "1", "0", "division by zero"),
        ("remainder-zero", "a % b", "1", "0", "division by zero"),
        (
            "divide-min-minus-one",
            "a / b",
            MIN,
            "-1",
            "integer overflow",
        ),
        (
            "remainder-min-minus-one",
            "a % b",
            MIN,
            "-1",
            "integer overflow",
        ),
    ];
    for (label, operation, left, right, message) in cases {
        let source = format!(
            r#"
async fn Calc(a: int, b: int): int! {{
    return ok({operation})
}}
async fn main(): null! {{
    child = Calc({left}, {right})
    ignored = await child
    println("BAD")
    return ok(null)
}}
"#
        );
        // Deliberately no '?' after await: an accidental ordinary Err would
        // let main print BAD and return ok(null), which both checks reject.
        compare_case(label, &source, "", Some(message));
    }
}
