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

fn lower_checked_ir(source: &str) -> ir::IrProgram {
    let tokens = Lexer::new(source).tokenize().expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new().check(&program).expect("check");
    ir::lower_program(&program).expect("lower ir")
}

fn generate_llvm(source: &str) -> Result<String, String> {
    let tokens = Lexer::new(source)
        .tokenize()
        .map_err(|err| err.to_string())?;
    let program = Parser::new(tokens).parse().map_err(|err| err.to_string())?;
    Checker::new()
        .check(&program)
        .map_err(|err| err.to_string())?;
    let ir = ir::lower_program(&program).map_err(|err| err.to_string())?;
    backend::llvm::generate_llvm_ir(&ir).map_err(|err| err.to_string())
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
fn native_c_backend_emits_simple_program_and_bounded_arrays() {
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
    let c = backend::c::generate_c_source(&lower_checked_ir(
        r#"
fn main() {
    values:[int] = [1, 2]
    print(values[0])
}
"#,
    ))
    .expect("generate bounded array C");
    assert!(complex.contains("[1, 2]"));
    assert!(c.contains("typedef struct { size_t len; int64_t* data; } KuArray_int;"));
    assert!(c.contains("ku_array_bounds_fail"));
    assert!(c.contains("ku_array_get_int"));
}

#[test]
fn llvm_backend_lowers_struct_values_and_field_assignment() {
    let llvm = generate_llvm(
        r#"
struct Pair {
    left:int
    right:int
}

fn sum(pair:Pair): int {
    return pair.left + pair.right
}

fn main() {
    pair = Pair { left: 2, right: 3 }
    pair.right = 5
    print(sum(pair))
}
"#,
    )
    .expect("generate LLVM");

    assert!(llvm.contains("%ku.struct.0.Pair = type { i64, i64 }"));
    assert!(llvm.contains("insertvalue %ku.struct.0.Pair undef, i64 2, 0"));
    assert!(llvm.contains("insertvalue %ku.struct.0.Pair"));
    assert!(llvm.contains("extractvalue %ku.struct.0.Pair"));
    assert!(llvm.contains("getelementptr inbounds %ku.struct.0.Pair"));
    assert!(llvm.contains("call i64 @ku_fn0_sum(%ku.struct.0.Pair"));
}

#[test]
fn llvm_backend_lowers_struct_result_question_and_propagation() {
    let llvm = generate_llvm(
        r#"
struct User {
    id:int
}

fn load(): User! {
    return ok(User { id: 7 })
}

fn reject(): User! {
    fail "rejected"
}

fn main(): User! {
    user = load()?
    user.id = user.id + 1
    return ok(user)
}
"#,
    )
    .expect("generate LLVM");

    assert!(llvm.contains("{ i1, %ku.struct.0.User, i8* }"));
    assert!(llvm.contains("extractvalue { i1, %ku.struct.0.User, i8* }"));
    assert!(llvm.contains("br i1"));
    assert!(llvm.contains("ret { i1, %ku.struct.0.User, i8* }"));
    assert!(llvm.contains("result.err:"));
    assert!(llvm.contains("ret i32 1"));
    assert!(!llvm.contains("  unreachable\n"));
    assert!(!llvm.contains("br label %entry"));
}

#[test]
fn llvm_backend_reports_unsupported_aggregate_boundaries() {
    let enum_err = generate_llvm(
        r#"
enum State {
    Ready
}
fn main() {
    print("ok")
}
"#,
    )
    .expect_err("enum layouts should remain explicit");
    assert!(enum_err.contains("does not support enum layouts"));

    let recursive = ir::IrProgram {
        functions: Vec::new(),
        layouts: ir::IrLayoutTable {
            structs: vec![ir::IrStructLayout {
                name: "Node".to_string(),
                fields: vec![ir::IrFieldLayout {
                    name: "next".to_string(),
                    ty: ir::IrType::Named("Node".to_string()),
                    offset: 0,
                }],
            }],
            enums: Vec::new(),
        },
    };
    let recursive_err = backend::llvm::generate_llvm_ir(&recursive)
        .expect_err("recursive value structs should remain explicit")
        .to_string();
    assert!(
        recursive_err.contains("recursive value struct layouts"),
        "unexpected error: {recursive_err}"
    );

    let result_err = generate_llvm(
        r#"
fn values(): [int]! {
    return ok([1, 2])
}
fn main() {
    print("ok")
}
"#,
    )
    .expect_err("array Result payload should remain explicit");
    assert!(
        result_err.contains("does not support Result<[int]>"),
        "unexpected error: {result_err}"
    );

    let self_loop = ir::IrProgram {
        functions: vec![ir::IrFunction {
            id: ir::FunctionId(0),
            name: "main".to_string(),
            params: Vec::new(),
            return_type: ir::IrType::Void,
            blocks: vec![ir::IrBlock {
                id: ir::BlockId(0),
                name: "entry".to_string(),
                instructions: Vec::new(),
                terminator: ir::IrTerminator::Jump(ir::BlockId(0)),
            }],
        }],
        layouts: ir::IrLayoutTable {
            structs: Vec::new(),
            enums: Vec::new(),
        },
    };
    let loop_err = backend::llvm::generate_llvm_ir(&self_loop)
        .expect_err("unconditional self-loop should be rejected")
        .to_string();
    assert!(
        loop_err.contains("unconditional self-jump"),
        "unexpected error: {loop_err}"
    );
}
