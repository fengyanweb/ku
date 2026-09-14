use ku::{
    backend::c::generate_c_source,
    checker::Checker,
    ir::{lower_program, optimize_program, verify_borrow_contract, IrType},
    lexer::Lexer,
    parser::Parser,
};
use std::{
    fs,
    path::PathBuf,
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[path = "support/bounded_process.rs"]
#[allow(dead_code)]
mod bounded_process;
use bounded_process::{run_bounded, OutputLimits};

const SOURCE: &str = r#"
struct User { name: str }
enum Message { Text(value: str) }
fn Identity<T>(value: T): T { return value }
fn Copy<T>(&value: T): T { return value.clone() }
fn Forward<T>(value: T): T { return Identity(value) }
fn Countdown<T>(value: T, count: int): T {
    if (count == 0) { return value }
    return Countdown(value, count - 1)
}
fn Read(&text: str): int { return text.len() }
fn Apply<T>(&op: fn(&T): T, &value: T): T { return op(value) }
fn CopyText(&text: str): str { return text.clone() }
fn main(): null! {
    text = "Ku" + "!"
    println(Copy(text))
    println(Copy(text))
    println(text)
    println(Identity(7))
    println(Identity(8))
    println(Countdown(Forward("done"), 3))
    values = Identity([1, 2, 3])
    println(values[1])
    result: str! = ok("result")
    println(Identity(result)?)
    user = Identity(User { name: "user" })
    println(user.name)
    message = Identity(Message.Text("message"))
    println(match message { Message.Text(value) => value })
    reader = Identity(Read)
    println(reader(text))
    println(Apply(CopyText, text))
    println(text)
    return ok(null)
}
"#;

fn lowered(source: &str) -> ku::ir::IrProgram {
    let program = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&program).unwrap();
    let ir = optimize_program(&lower_program(&program).unwrap());
    verify_borrow_contract(&ir).unwrap();
    ir
}

#[test]
fn native_generic_concrete_instances_are_deduplicated_stable_and_borrowed() {
    let first = lowered(SOURCE);
    let second = lowered(SOURCE);
    assert_eq!(first, second);
    let identity_int = first
        .functions
        .iter()
        .filter(|function| {
            function.name.contains("Identity")
                && function.params.len() == 1
                && function.params[0].ty == IrType::Int
        })
        .count();
    assert_eq!(identity_int, 1);
    let c = generate_c_source(&first).unwrap();
    assert!(c.contains("const KuString*"));
    assert!(!c.contains("KuStruct_T"));
    assert!(!c.contains("run_source") && !c.contains("const SOURCE"));
}

#[test]
fn native_generic_type_growing_recursion_is_rejected_with_a_bounded_diagnostic() {
    let source = "fn Grow<T>(value: T, count: int) { if (count > 0) { Grow([value], count - 1) } } fn main() { Grow(1, 1) }";
    let program = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    let error = lower_program(&program).unwrap_err();
    assert!(error.message.contains("generic"), "{error}");
    assert!(
        error.message.contains("limit") || error.message.contains("depth"),
        "{error}"
    );
}

#[test]
fn native_generic_concrete_nominal_type_is_not_substituted_twice() {
    let source = "struct T { name: str } fn Keep<A, T>(value: A, ignored: T): A { local: A = value return local } fn main() { value = Keep(T { name: \"nominal\" }, 1) println(value.name) }";
    let ir = lowered(source);
    let function = ir
        .functions
        .iter()
        .find(|function| function.name.contains("Keep"))
        .unwrap();
    assert_eq!(function.params[0].ty, IrType::Named("T".into()));
    assert_eq!(function.params[1].ty, IrType::Int);
    assert_eq!(function.return_type, IrType::Named("T".into()));
    generate_c_source(&ir).unwrap();
}

#[test]
fn native_generic_unresolved_calls_fail_before_erasing_the_template() {
    let source = "fn Unused<T>(): int { return 1 } fn main() { println(Unused()) }";
    let program = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    let error = Checker::new().check(&program).unwrap_err();
    assert!(
        error.message.contains("could not infer generic type"),
        "{error}"
    );
    let error = lower_program(&program).unwrap_err();
    assert!(
        error.message.contains("could not infer generic type"),
        "{error}"
    );

    // T is inferred here, but its empty-array element remains Unknown. This
    // reaches the planner's deferred-call guard, rather than the checker's
    // existing missing-type-parameter rejection above.
    let source = "fn Identity<T>(value: T): T { return value } fn main() { Identity([]) }";
    let program = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&program).unwrap();
    let error = lower_program(&program).unwrap_err();
    assert!(
        error.message.contains("cannot resolve concrete arguments"),
        "{error}"
    );
    assert!(error.message.contains("Identity"), "{error}");
}

#[test]
fn native_generic_local_declarations_inside_callbacks_are_explicitly_rejected() {
    for source in [
        "fn main() { op = fn(): int { fn Local<T>(value: T): T { return value } return 1 } println(op()) }",
        "fn Identity<T>(value: T): T { return value } fn main() { op = fn(): int { fn Local<T>(value: T): T { return value } return 1 } println(op()) }",
        "fn main() { values = [fn(): int { fn Local<T>(value: T): T { return value } return 1 }] }",
    ] {
        let program = Parser::new(Lexer::new(source).lex().unwrap()).parse_program().unwrap();
        let error = lower_program(&program).unwrap_err();
        assert!(error.message.contains("local generic functions"), "{error}\n{source}");
    }
}

struct Fixture(PathBuf);

#[test]
fn native_generic_instance_count_limit_is_exact_and_repeated_calls_are_free() {
    fn source(types: usize, repeat_first: bool) -> String {
        let mut source = String::from("fn Id<T>(value: T): T { return value }\n");
        for index in 0..types {
            source.push_str(&format!("struct S{index} {{ value: int }}\n"));
        }
        source.push_str("fn main() {\n");
        for index in 0..types {
            source.push_str(&format!("Id(S{index} {{ value: {index} }})\n"));
        }
        if repeat_first {
            source.push_str("Id(S0 { value: 7 })\n");
        }
        source.push_str("}\n");
        source
    }
    let ir = lowered(&source(256, true));
    assert_eq!(
        ir.functions
            .iter()
            .filter(|function| function.name.starts_with("__ku_ns_generic_"))
            .count(),
        256
    );
    let over = source(257, false);
    let program = Parser::new(Lexer::new(&over).lex().unwrap())
        .parse_program()
        .unwrap();
    let error = lower_program(&program).unwrap_err();
    assert!(
        error
            .message
            .contains("generic specialization limit exceeded: instance count"),
        "{error}"
    );
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

#[test]
fn native_generic_matches_interpreter_after_sources_are_removed() {
    let dir = Fixture(std::env::temp_dir().join(format!(
            "ku-generic-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
    fs::create_dir(&dir.0).unwrap();
    let source = dir.0.join("main.ku");
    fs::write(&source, SOURCE).unwrap();
    let limits = OutputLimits::new(2 * 1024 * 1024, 4 * 1024 * 1024);
    let interpreted = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_ku"))
            .current_dir(&dir.0)
            .args(["run", "main.ku"]),
        Duration::from_secs(20),
        limits,
    )
    .unwrap();
    assert!(
        interpreted.status.success(),
        "{}",
        String::from_utf8_lossy(&interpreted.stderr)
    );
    let executable = if cfg!(windows) { "out.exe" } else { "out" };
    let built = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_ku"))
            .current_dir(&dir.0)
            .args(["build", "--native", "main.ku", "-o", executable]),
        Duration::from_secs(120),
        limits,
    )
    .unwrap();
    if !built.status.success() {
        let error = String::from_utf8_lossy(&built.stderr);
        if error.contains("C compiler not found") {
            eprintln!(
                "C artifact remains mandatory; native execution skipped without a C compiler"
            );
            return;
        }
        panic!(
            "stdout:\n{}\nstderr:\n{error}",
            String::from_utf8_lossy(&built.stdout)
        );
    }
    fs::remove_file(source).unwrap();
    let moved = dir
        .0
        .join(if cfg!(windows) { "moved.exe" } else { "moved" });
    fs::rename(dir.0.join(executable), &moved).unwrap();
    let native = run_bounded(
        Command::new(moved).current_dir(&dir.0),
        Duration::from_secs(20),
        limits,
    )
    .unwrap();
    assert!(
        native.status.success(),
        "{}",
        String::from_utf8_lossy(&native.stderr)
    );
    assert_eq!(
        String::from_utf8(native.stdout)
            .unwrap()
            .replace("\r\n", "\n"),
        String::from_utf8(interpreted.stdout)
            .unwrap()
            .replace("\r\n", "\n")
    );
}
