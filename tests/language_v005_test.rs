use std::{
    env, fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use ku::ast::{ExprKind, Item, Stmt};
use ku::cli::{check_source, run_source};
use ku::lexer::Lexer;
use ku::parser::Parser;
use ku::token::TokenKind;

fn unique_temp_dir(name: &str) -> PathBuf {
    env::temp_dir().join(format!(
        "ku-{name}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should work")
            .as_nanos()
    ))
}

fn check_err(source: &str) -> String {
    check_source("inline.ku", source)
        .expect_err("program should fail")
        .to_string()
}

fn run_err(source: &str) -> String {
    run_source("inline.ku", source)
        .expect_err("program should fail")
        .to_string()
}

#[test]
fn lexer_tokenizes_v005_error_flow_keywords() {
    let tokens = Lexer::new(r#"try { fail "bad"? } catch (err) { panic err } finally { }"#)
        .tokenize()
        .expect("lex should pass");

    assert!(tokens
        .iter()
        .any(|token| matches!(token.kind, TokenKind::Try)));
    assert!(tokens
        .iter()
        .any(|token| matches!(token.kind, TokenKind::Catch)));
    assert!(tokens
        .iter()
        .any(|token| matches!(token.kind, TokenKind::Finally)));
    assert!(tokens
        .iter()
        .any(|token| matches!(token.kind, TokenKind::Fail)));
    assert!(tokens
        .iter()
        .any(|token| matches!(token.kind, TokenKind::Panic)));
    assert!(tokens
        .iter()
        .any(|token| matches!(token.kind, TokenKind::Question)));
}

#[test]
fn parser_builds_result_types_try_and_question_nodes() {
    let source = r#"
fn load(): str! {
    return ok("ready")
}

fn main() {
    try {
        print(load()?)
    } catch (err) {
        print(err.message)
    } finally {
        print("done")
    }
}
"#;
    let tokens = Lexer::new(source).tokenize().expect("lex should pass");
    let program = Parser::new(tokens)
        .parse_program()
        .expect("parse should pass");

    let Item::Function(load) = &program.items[0] else {
        panic!("expected load function");
    };
    assert!(load.return_type.is_some(), "expected Result return type");

    let Item::Function(main) = &program.items[1] else {
        panic!("expected main function");
    };
    let Stmt::Try { body, .. } = &main.body[0] else {
        panic!("expected try statement");
    };
    let Stmt::Print { value, .. } = &body[0] else {
        panic!("expected print statement");
    };
    assert!(matches!(value.kind, ExprKind::TryUnwrap { .. }));
}

#[test]
fn checker_and_interpreter_handle_result_question_and_try_catch_finally() {
    let source = r#"
fn read_name(): str! {
    fail "missing name"
}

fn main() {
    message = "none"
    try {
        message = read_name()?
    } catch (err) {
        message = "caught " + err.message
    } finally {
        message = message + " finally"
    }
    print(message)
}
"#;

    check_source("inline.ku", source).expect("program should check");
    run_source("inline.ku", source).expect("program should run");
}

#[test]
fn question_can_propagate_through_result_functions() {
    let source = r#"
fn value(): int! {
    return ok(7)
}

fn plus_one(): int! {
    return ok(value()? + 1)
}

fn main() {
    result = plus_one()
    print(result)
}
"#;

    check_source("inline.ku", source).expect("program should check");
    run_source("inline.ku", source).expect("program should run");
}

#[test]
fn question_short_circuits_the_rest_of_the_expression() {
    let source = r#"
fn missing(): str! {
    fail "missing"
}

fn boom(): str {
    panic "side effect ran"
    return "bad"
}

fn main() {
    message = "none"
    try {
        message = missing()? + boom()
    } catch (err) {
        message = err.message
    }
    print(message)
}
"#;

    check_source("inline.ku", source).expect("program should check");
    run_source("inline.ku", source).expect("program should run");
}

#[test]
fn try_read_returns_recoverable_result_for_missing_files() {
    let source = r#"
import "std.fs"

fn load(): str! {
    return ok(fs.try_read("definitely-missing-ku-file.txt")?)
}

fn main() {
    message = "none"
    try {
        message = load()?
    } catch (err) {
        message = "missing"
    }
    print(message)
}
"#;

    check_source("inline.ku", source).expect("program should check");
    run_source("inline.ku", source).expect("program should run");
}

#[test]
fn checker_rejects_invalid_error_flow() {
    let naked_question = check_err(
        r#"
fn value(): int! { return ok(1) }
fn main() { print(value()?) }
"#,
    );
    assert!(
        naked_question.contains("'?' requires"),
        "unexpected error: {naked_question}"
    );

    let wrong_fail_value = check_err(
        r#"
fn value(): int! { fail 123 }
fn main() { print(value()) }
"#,
    );
    assert!(
        wrong_fail_value.contains("expected object but got int"),
        "unexpected error: {wrong_fail_value}"
    );

    let wrong_question_target = check_err(
        r#"
fn main() {
    try {
        print(1?)
    } catch (err) {
        print(err.message)
    }
}
"#,
    );
    assert!(
        wrong_question_target.contains("'?' expects Result"),
        "unexpected error: {wrong_question_target}"
    );
}

#[test]
fn unhandled_recoverable_error_and_panic_are_runtime_errors() {
    let unhandled = run_err(
        r#"
fn broken(): str! { fail "bad" }
fn main() {
    try {
        broken()?
    } finally {
        print("cleanup")
    }
}
"#,
    );
    assert!(
        unhandled.contains("unhandled recoverable error:")
            && unhandled.contains("message")
            && unhandled.contains("bad"),
        "unexpected error: {unhandled}"
    );

    let panic_error = run_err(
        r#"
fn main() {
    panic "boom"
}
"#,
    );
    assert!(
        panic_error.contains("panic: boom"),
        "unexpected error: {panic_error}"
    );
}

#[test]
fn match_guards_are_checked_and_run() {
    let source = r#"
enum Result {
    Ok(value: int)
    Err(message: str)
}

fn main() {
    value = Result.Ok(3)
    text = match value {
        Result.Ok(n) if n > 2 => "large"
        Result.Ok(n) => str(n)
        Result.Err(message) => message
    }
    print(text)
}
"#;

    check_source("inline.ku", source).expect("program should check");
    run_source("inline.ku", source).expect("program should run");

    let err = check_err(
        r#"
enum Result { Ok(value:int) }
fn main() {
    value = Result.Ok(1)
    text = match value {
        Result.Ok(n) if "bad" => str(n)
    }
}
"#,
    );
    assert!(err.contains("expected bool"), "unexpected error: {err}");
}

#[test]
fn namespace_imports_rewrite_result_types_try_and_match_guards() {
    let dir = unique_temp_dir("v005-namespace");
    fs::create_dir_all(&dir).expect("create temp dir");
    let lib_path = dir.join("math.ku");
    let main_path = dir.join("main.ku");
    fs::write(
        &lib_path,
        r#"
enum Boxed {
    Value(value: int)
}

fn Load(): int! {
    return ok(3)
}

fn IsLarge(value: int): bool {
    return value > 2
}
"#,
    )
    .expect("write lib");

    let source = r#"
import math from "./math.ku"

fn main() {
    boxed = math.Boxed.Value(3)
    try {
        value = math.Load()?
        text = match boxed {
            math.Boxed.Value(n) if math.IsLarge(n) => str(value + n)
            _ => "small"
        }
        print(text)
    } catch (err) {
        print(err.message)
    }
}
"#;
    fs::write(&main_path, source).expect("write main");

    let main_name = main_path.to_string_lossy();
    check_source(&main_name, source).expect("namespace program should check");
    run_source(&main_name, source).expect("namespace program should run");
}
