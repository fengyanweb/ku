use std::{
    env, fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use ku::{backend, checker::Checker, cli::run_cli, ir, lexer::Lexer, package, parser::Parser};

fn unique_temp_path(name: &str) -> PathBuf {
    env::temp_dir().join(format!(
        "ku-v009-{name}-{}",
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

#[test]
fn ir_records_struct_and_enum_layouts() {
    let text = lower_ir(
        r#"
struct User {
    name:str
    age:int
}

enum Result {
    Ok(value:int)
    Err(message:str)
}

fn main() {}
"#,
    );

    assert!(text.contains("struct User {name@0: str, age@1: int}"));
    assert!(text.contains("enum Result"));
    assert!(text.contains("#0 Ok(value@0: int)"));
    assert!(text.contains("#1 Err(message@0: str)"));
}

#[test]
fn ir_local_function_captures_only_free_variables() {
    let text = lower_ir(
        r#"
fn main() {
    outer = 10
    fn add(x:int): int {
        y = x + outer
        return y
    }
}
"#,
    );

    assert!(
        text.contains("closure add = fn#10000 captures [outer]"),
        "unexpected IR:\n{text}"
    );
    assert!(!text.contains("captures [outer, x"));
    assert!(!text.contains("captures [outer, y"));
}

#[test]
fn package_version_writes_lock_file() {
    let dir = unique_temp_path("package-lock");
    let src = dir.join("src");
    fs::create_dir_all(&src).expect("create package src");
    fs::write(
        dir.join("ku.mod"),
        r#"
name = "demo_pkg"
version = "0.1.2"
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

    let context = package::discover_for_file(&main)
        .expect("discover")
        .expect("package should exist");
    assert_eq!(context.manifest.version.as_deref(), Some("0.1.2"));
    run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect("package check");
    let lock = fs::read_to_string(dir.join("ku.lock")).expect("read lock");
    assert!(lock.contains("package = \"demo_pkg\""));
    assert!(lock.contains("version = \"0.1.2\""));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn invalid_package_version_is_rejected() {
    let err = package::parse_manifest(
        r#"
name = "demo_pkg"
version = "dev"
"#,
        Default::default(),
    )
    .expect_err("invalid version should fail")
    .to_string();
    assert!(err.contains("major.minor.patch"), "unexpected error: {err}");
}

#[test]
fn native_c_backend_emits_simple_program_and_rejects_complex_nodes() {
    let source = r#"
fn add(a:int,b:int): int {
    return a + b
}

fn main() {
    print(add(1, 2))
}
"#;
    let tokens = Lexer::new(source).tokenize().expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new().check(&program).expect("check");
    let ir = ir::lower_program(&program).expect("lower ir");
    let c = backend::c::generate_c_source(&ir).expect("generate c");
    assert!(c.contains("int64_t add"));
    assert!(c.contains("printf"));

    let complex = lower_ir(
        r#"
fn main() {
    values:[int] = [1, 2]
    print(values[0])
}
"#,
    );
    let tokens = Lexer::new(
        r#"
fn main() {
    values:[int] = [1, 2]
    print(values[0])
}
"#,
    )
    .tokenize()
    .expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new().check(&program).expect("check");
    let ir = ir::lower_program(&program).expect("lower ir");
    let err = backend::c::generate_c_source(&ir)
        .expect_err("arrays are outside prototype")
        .to_string();
    assert!(complex.contains("[1, 2]"));
    assert!(
        err.contains("native C prototype"),
        "unexpected error: {err}"
    );
}
