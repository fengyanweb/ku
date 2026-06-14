use std::{
    env, fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use ku::{
    backend, checker::Checker, cli::run_cli, cli::run_source, ir, lexer::Lexer, parser::Parser,
};

fn unique_temp_path(name: &str) -> PathBuf {
    env::temp_dir().join(format!(
        "ku-v010-{name}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should work")
            .as_nanos()
    ))
}

fn lower_ir(source: &str) -> String {
    let tokens = Lexer::new(source).tokenize().expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new().check(&program).expect("check");
    ir::lower_program(&program).expect("lower ir").to_string()
}

fn check_err(source: &str) -> String {
    let tokens = Lexer::new(source).tokenize().expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new()
        .check(&program)
        .expect_err("program should fail")
        .to_string()
}

#[test]
fn runtime_closure_captures_outer_bindings_without_whole_env() {
    let source = r#"
fn main() {
    base = 1
    fn calc(n:int): int {
        if (n <= 1) {
            return base
        } else {
            return n * calc(n - 1)
        }
    }
    base = 2
    value = calc(4)
    if (value != 48) {
        panic("bad closure capture")
    }
}
"#;

    run_source("inline.ku", source).expect("recursive local closure should run");
}

#[test]
fn ir_lowers_question_to_explicit_result_cfg() {
    let text = lower_ir(
        r#"
fn value(): int! {
    return ok(7)
}

fn main(): int! {
    item = value()?
    return ok(item)
}
"#,
    );

    assert!(text.contains("result_branch"), "unexpected IR:\n{text}");
    assert!(text.contains("ok_value"), "unexpected IR:\n{text}");
    assert!(text.contains("propagate_err"), "unexpected IR:\n{text}");
    assert!(!text.contains(" = value()?"), "unexpected IR:\n{text}");
}

#[test]
fn checker_requires_enum_match_to_be_exhaustive() {
    let err = check_err(
        r#"
enum Maybe {
    Some(value:int)
    None
}

fn main() {
    value = Maybe.Some(1)
    text = match value {
        Maybe.Some(v) => "some"
    }
    print(text)
}
"#,
    );
    assert!(err.contains("not exhaustive"), "unexpected error: {err}");

    let guarded = check_err(
        r#"
enum Maybe {
    Some(value:int)
    None
}

fn main() {
    value = Maybe.Some(1)
    text = match value {
        Maybe.Some(v) if (v > 0) => "some"
        Maybe.None => "none"
    }
    print(text)
}
"#,
    );
    assert!(
        guarded.contains("not exhaustive"),
        "unexpected error: {guarded}"
    );
}

#[test]
fn native_c_backend_accepts_if_while_int_subset() {
    let tokens = Lexer::new(
        r#"
fn sum(n:int): int {
    total = 0
    i = 0
    while (i < n) {
        total = total + i
        i = i + 1
    }
    if (total > 2) {
        return total
    } else {
        return 0
    }
}

fn main() {
    print(sum(4))
}
"#,
    )
    .tokenize()
    .expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new().check(&program).expect("check");
    let ir = ir::lower_program(&program).expect("lower");
    let c = backend::c::generate_c_source(&ir).expect("generate c");

    assert!(c.contains("if ("));
    assert!(c.contains("goto block"));
    assert!(c.contains("block"));
    assert!(c.contains("return total;"));
}

#[test]
fn package_lock_records_import_dependencies_and_cache_keys() {
    let dir = unique_temp_path("package-lock-deps");
    let src = dir.join("src");
    fs::create_dir_all(&src).expect("create package src");
    fs::write(
        dir.join("ku.mod"),
        r#"
name = "demo_pkg"
version = "0.1.3"
"#,
    )
    .expect("write ku.mod");
    fs::write(src.join("util.ku"), "fn Value(): int { return 1 }").expect("write util");
    let main = src.join("main.ku");
    fs::write(
        &main,
        r#"
import { Value } from "util"
fn main() { print(Value()) }
"#,
    )
    .expect("write main");

    run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect("package check");
    let lock = fs::read_to_string(dir.join("ku.lock")).expect("read lock");
    assert!(lock.contains("[[dependency]]"), "unexpected lock:\n{lock}");
    assert!(
        lock.contains("path = \"src/util.ku\""),
        "unexpected lock:\n{lock}"
    );
    assert!(
        lock.contains("cache_key = \"ku-fnv64-"),
        "unexpected lock:\n{lock}"
    );

    let _ = fs::remove_dir_all(dir);
}
