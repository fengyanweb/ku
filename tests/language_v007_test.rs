use std::{
    env, fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use ku::{
    checker::Checker,
    cli::{check_source, run_cli, run_source},
    ir,
    lexer::Lexer,
    package::{self, DEFAULT_CACHE_DIR},
    parser::Parser,
};

fn unique_temp_path(name: &str) -> PathBuf {
    env::temp_dir().join(format!(
        "ku-v007-{name}-{}",
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

#[test]
fn array_try_get_and_string_slice_work_with_question() {
    let source = r#"
fn get_value(): int! {
    values:[int] = [10, 20]
    return ok(array.try_get(values, 1)?)
}

fn get_text(): str! {
    return ok(string.slice("你好Ku", 0, 2)?)
}

fn main() {
    try {
        print(get_value()?)
        print(get_text()?)
    } catch (err) {
        print(err)
    }
}
"#;

    check_source("inline.ku", source).expect("try_get/slice should check");
    run_source("inline.ku", source).expect("try_get/slice should run");
}

#[test]
fn try_get_and_slice_errors_are_recoverable_results() {
    let source = r#"
fn main() {
    message = "none"
    try {
        value = array.try_get([1], 9)?
        print(value)
    } catch (err) {
        message = err
    }
    print(message)

    try {
        text = string.slice("Ku", 2, 9)?
        print(text)
    } catch (err) {
        message = err
    }
    print(message)
}
"#;

    check_source("inline.ku", source).expect("recoverable stdlib errors should check");
    run_source("inline.ku", source).expect("recoverable stdlib errors should run");
}

#[test]
fn question_short_circuits_conditions_before_side_effects() {
    let source = r#"
fn fail_bool(): bool! {
    return err("bad condition")
}

fn main() {
    message = "not caught"
    try {
        if (fail_bool()?) {
            message = "bad"
        }
        message = "side effect"
    } catch (err) {
        message = err
    }
    print(message)
}
"#;

    check_source("inline.ku", source).expect("condition ? should check");
    run_source("inline.ku", source).expect("condition ? should run");
}

#[test]
fn stdlib_metadata_catches_new_type_errors_before_run() {
    for source in [
        r#"fn main() { print(array.try_get([1], "bad")) }"#,
        r#"fn main() { print(string.slice("Ku", 0, "bad")) }"#,
        r#"fn main() { value:str = array.try_get([1], 0)? }"#,
    ] {
        let err = check_err(source);
        assert!(
            err.contains("type error") || err.contains("'?'"),
            "unexpected error: {err}"
        );
    }
}

#[test]
fn package_manifest_sets_import_root_and_cache() {
    let dir = unique_temp_path("package");
    let src = dir.join("src");
    fs::create_dir_all(&src).expect("create package src");
    fs::write(
        dir.join("ku.mod"),
        r#"
name = "demo_pkg"
"#,
    )
    .expect("write ku.mod");
    fs::write(
        src.join("util.ku"),
        r#"
fn Value(): int {
    return 7
}
"#,
    )
    .expect("write util");
    let main = src.join("main.ku");
    fs::write(
        &main,
        r#"
import { Value } from "util"

fn main() {
    print(Value())
}
"#,
    )
    .expect("write main");

    let package = package::discover_for_file(&main)
        .expect("package discovery should work")
        .expect("package should exist");
    assert_eq!(package.manifest.name, "demo_pkg");
    assert!(package.cache_dir.ends_with(DEFAULT_CACHE_DIR));
    run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect("package root import should check");
    assert!(
        package.cache_dir.exists(),
        "package cache directory should be created"
    );

    let outside = dir.join("outside.ku");
    fs::write(&outside, "fn Value(): int { return 1 }").expect("write outside");
    fs::write(
        &main,
        r#"
import { Value } from "../outside.ku"
fn main() { print(Value()) }
"#,
    )
    .expect("write outside import");
    let err = run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect_err("outside package import should fail")
    .to_string();
    assert!(
        err.contains("outside package import root"),
        "unexpected error: {err}"
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn ir_lowering_produces_checked_function_ir() {
    let source = r#"
fn add(a:int,b:int): int {
    result = a + b
    return result
}

fn main() {
    print(add(1, 2))
}
"#;
    let tokens = Lexer::new(source).tokenize().expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new().check(&program).expect("check");
    let ir = ir::lower_program(&program).expect("lower ir");
    let text = ir.to_string();
    assert!(text.contains("fn add(a: int, b: int) -> int"));
    assert!(text.contains("%t0: int = a + b"));
    assert!(text.contains("let result: int = %t0"));
    assert!(text.contains("return result"));
}

#[test]
fn ir_lowering_emits_typed_cfg_and_lvalues() {
    let source = r#"
fn main() {
    values:[int] = [1, 2]
    values[0] = 9
    if (values[0] > 1) {
        print(values[0])
    } else {
        print(0)
    }
}
"#;
    let tokens = Lexer::new(source).tokenize().expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new().check(&program).expect("check");
    let text = ir::lower_program(&program).expect("lower ir").to_string();

    assert!(text.contains("%t0: [int] = [1, 2]"));
    assert!(text.contains("let values: [int] = %t0"));
    assert!(text.contains("store values[0] = 9"));
    assert!(text.contains("branch %t"));
    assert!(text.contains("jump block"));
}
