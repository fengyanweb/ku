use ku::{checker::Checker, cli::run_cli, error::KuError, ir, lexer::Lexer, parser::Parser};
use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

fn checked(source: &str) -> ku::ast::Program {
    let program = Parser::new(Lexer::new(source).lex().expect("fixture lexes"))
        .parse_program()
        .expect("fixture uses supported Ku syntax");
    Checker::new()
        .check(&program)
        .unwrap_or_else(|error| panic!("fixture checks before native boundary: {error}\n{source}"));
    program
}

fn assert_async_rejection(error: KuError) {
    assert!(error.message.contains("async"), "{error}");
    assert!(
        error.message.contains("not supported") || error.message.contains("does not support"),
        "native must explain its unsupported async boundary: {error}"
    );
}

fn rejects_lowering(source: &str) {
    let program = checked(source);
    let error = ir::lower_program(&program)
        .expect_err("async must not silently lower to synchronous native IR");
    assert_async_rejection(error);
}

#[test]
fn native_async_boundary_direct_top_level_without_await() {
    for source in [
        "async fn main(): null! { println(1) return ok(null) }",
        "async fn Load(): int! { return ok(1) } fn main() {}",
    ] {
        rejects_lowering(source);
    }
}

#[test]
fn native_async_boundary_direct_local_without_await() {
    rejects_lowering("fn main() { async fn Load(): int! { return ok(1) } }");
}

#[test]
fn native_async_boundary_anonymous_body_contains_local_async_without_await() {
    // Ku has synchronous function literals, not an `async fn(...)` literal.
    // Their bodies may contain ordinary local async function declarations.
    for source in [
        "fn main() { wrapper = fn() { async fn Load(): int! { return ok(1) } } wrapper() }",
        "fn main() { wrapper = () => { async fn Load(): int! { return ok(1) } } wrapper() }",
    ] {
        rejects_lowering(source);
    }
}

#[test]
fn native_async_boundary_unused_generic_is_rejected_before_specialization() {
    rejects_lowering("async fn Load<T>(value: T): T! { return ok(value) } fn main() {}");
}

#[test]
fn native_async_boundary_function_type_does_not_lose_async_flag() {
    for source in [
        "fn Keep(callback: async fn(): int!) {} fn main() {}",
        "fn Keep(callbacks: [async fn(): int!]) {} fn main() {}",
        "struct Holder { callback: async fn(): int! } fn main() {}",
        "enum Handler { Callback(callback: async fn(): int!) } fn main() {}",
    ] {
        rejects_lowering(source);
    }
}

struct FixtureDir(PathBuf);

impl FixtureDir {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ku-native-async-boundary-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create private fixture directory");
        Self(path)
    }
}

impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn native_async_boundary_import_expanded_ir_without_await() {
    let fixture = FixtureDir::new();
    fs::write(
        fixture.0.join("worker.ku"),
        "async fn Load(): int! { return ok(1) }",
    )
    .expect("write imported async function");
    for source in [
        "import { Load } from \"./worker.ku\"\nfn main() { callback = Load }",
        "import worker from \"./worker.ku\"\nfn main() { callback = worker.Load }",
    ] {
        let main = fixture.0.join("main.ku");
        fs::write(&main, source).expect("write importing entry");
        ku::cli::check_source(&main.to_string_lossy(), source)
            .expect("imported async source is valid interpreted Ku");
        let error = run_cli(vec![
            "ku".to_string(),
            "ir".to_string(),
            main.to_string_lossy().into_owned(),
        ])
        .expect_err("IR must reject async after the real import graph is expanded");
        assert_async_rejection(error);
    }
}

#[test]
fn native_async_boundary_sync_names_comments_and_strings_are_not_rejected() {
    let program = checked(
        "// async fn and await here are trivia\nfn asynchronous(): int { return 1 } fn main() { async_value = asynchronous() println(async_value) println(\"async fn await\") }",
    );
    ir::lower_program(&program).expect("synchronous code remains native-lowerable");
}

#[test]
fn native_async_boundary_template_expression_types_are_checked_after_parsing() {
    rejects_lowering("fn main() { println(`{(op: async fn(): int!): int => 1}`) }");
    let program = checked("fn main() { value = 7 println(`async fn await {value + 1}`) }");
    ir::lower_program(&program)
        .expect("synchronous template text and expressions remain supported");
}

#[test]
fn native_async_boundary_cli_native_llvm_and_emit_ir_reject_before_artifacts() {
    let fixture = FixtureDir::new();
    let main = fixture.0.join("main.ku");
    fs::write(&main, "fn Keep(callback: async fn(): int!) {} fn main() {}")
        .expect("write native boundary fixture");
    for (arguments, message) in [
        (
            vec!["build", "--native"],
            "native C prototype does not support async/await yet",
        ),
        (
            vec!["build", "--emit-ir"],
            "async/await is not supported by IR/native lowering yet",
        ),
        (
            vec!["llvm"],
            "LLVM text prototype does not support async/await yet",
        ),
    ] {
        let mut command = vec!["ku".to_string()];
        command.extend(arguments.into_iter().map(str::to_string));
        command.push(main.to_string_lossy().into_owned());
        let error = run_cli(command).expect_err("compiled command must reject unsupported async");
        assert!(error.message.contains(message), "{error}");
    }
    let mut directories = vec![fixture.0.clone()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory).expect("inspect private output directory") {
            let entry = entry.expect("fixture output entry");
            if entry.file_type().expect("fixture file type").is_dir() {
                directories.push(entry.path());
            } else {
                let path = entry.path();
                let extension = path.extension().and_then(|value| value.to_str());
                assert!(
                    !matches!(
                        extension,
                        Some("c" | "ir" | "ll" | "o" | "obj" | "exe" | "rs")
                    ) && path.file_name().and_then(|value| value.to_str()) != Some("main"),
                    "rejected commands must not write a compiled artifact: {}",
                    path.display()
                );
            }
        }
    }
}
