use std::{
    env, fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use ku::{
    backend,
    checker::Checker,
    cli::{check_source, run_cli, run_source},
    ir,
    lexer::Lexer,
    package,
    parser::Parser,
};

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

    // Stage 6f: a local named function lowers through the closure machinery — its
    // body is lifted into a `__ku_closure_N` and the name binds a closure value.
    // Only the free variable `outer` is captured (boxed into a shared cell and
    // read via `captured_cell`); the parameter `x` and the local `y` are not.
    assert!(text.contains("let add: closure"), "unexpected IR:\n{text}");
    assert!(text.contains("cell_new outer"), "unexpected IR:\n{text}");
    assert!(
        text.contains("captured_cell outer"),
        "outer must be captured:\n{text}"
    );
    assert!(
        !text.contains("captured_cell x") && !text.contains("cell_new x"),
        "parameter x must not be captured:\n{text}"
    );
    assert!(
        !text.contains("captured_cell y") && !text.contains("cell_new y"),
        "local y must not be captured:\n{text}"
    );
}

#[test]
fn runtime_assignment_targets_capture_outer_cells_and_keep_missing_names_local() {
    let source = r#"
fn main() {
    direct = 1
    set_direct = () => {
        direct = 7
        return null
    }
    set_direct()
    if (direct != 7) {
        panic("plain assignment did not update the outer cell")
    }

    left = 2
    right = 3
    set_pair = () => {
        left, right = 8, 9
        return null
    }
    set_pair()
    if (left != 8 || right != 9) {
        panic("destructuring assignment did not update outer cells")
    }

    object_code = 4
    object_default = 5
    set_object = () => {
        { code: object_code, missing: object_default = 11 } = { code: 10 }
        return null
    }
    set_object()
    if (object_code != 10 || object_default != 11) {
        panic("object destructuring did not update outer cells")
    }

    named_value = 6
    run_named = () => {
        fn set_named() {
            named_value = 12
        }
        set_named()
        return null
    }
    run_named()
    if (named_value != 12) {
        panic("nested local function did not forward its outer capture")
    }

    literal_value = 7
    run_literal = () => {
        set_literal = () => {
            literal_value = 13
            return null
        }
        set_literal()
        return null
    }
    run_literal()
    if (literal_value != 13) {
        panic("nested closure did not forward its outer capture")
    }

    named_parent_local = () => {
        state: int = 14
        fn set_state() {
            state = 22
            return null
        }
        set_state()
        return state
    }
    if (named_parent_local() != 22) {
        panic("nested local function did not update its immediate parent's local")
    }

    literal_parent_local = () => {
        state: int = 15
        set_state = () => {
            state = 23
            return null
        }
        set_state()
        return state
    }
    if (literal_parent_local() != 23) {
        panic("nested closure did not update its immediate parent's local")
    }

    use_local = (seed: int) => {
        local: int = seed
        fn bump(): int {
            local = local + 1
            return local
        }
        return bump()
    }
    if (use_local(40) != 41) {
        panic("nested function parameter or local was misclassified as an outer capture")
    }

    local_writer = () => {
        fresh = 21
        return fresh
    }
    if (local_writer() != 21) {
        panic("missing capture candidate was not created locally")
    }
    fresh = 34
    if (fresh != 34) {
        panic("closure-local assignment contaminated its caller")
    }
}
"#;

    check_source("capture-assignment.ku", source).expect("assignment captures should check");
    run_source("capture-assignment.ku", source).expect("assignment captures should run");
}

#[test]
fn ir_boxing_uses_the_binding_visible_at_closure_creation() {
    let source = r#"
fn main() {
    captured = 1
    set_captured = () => {
        captured = 2
        return captured
    }

    local_writer = () => {
        fresh = 21
        return fresh
    }

    // `fresh` does not exist when local_writer is created. A later homonym,
    // including an IR-unsupported union payload, must remain an ordinary local.
    fresh: str | int = 34

    parameter_shadow = (captured: int) => {
        return captured
    }
    local_shadow = () => {
        captured: int = 41
        return captured
    }

    fn countdown(n: int): int {
        local = n
        if (n <= 0) {
            return local
        }
        return countdown(n - 1)
    }

    if (set_captured() != 2 || captured != 2) {
        panic("outer assignment-only capture lost its shared cell")
    }
    if (local_writer() != 21 || parameter_shadow(7) != 7 || local_shadow() != 41) {
        panic("a closure-local binding escaped its lexical scope")
    }
    if (countdown(3) != 0) {
        panic("local function self recursion was treated as a capture")
    }
}
"#;

    check_source("lexical-boxing.ku", source).expect("lexical boxing source should check");
    run_source("lexical-boxing.ku", source).expect("lexical boxing source should run");

    // Before the scope-aware scan, `fresh` from local_writer's assignment was
    // merged into main's name-only candidate set. Lowering then tried to box the
    // later union-typed `fresh` and failed with an unsupported `unknown` cell.
    let text = lower_ir(source);
    assert_eq!(
        text.matches("cell_new captured").count(),
        1,
        "the genuine outer capture must be boxed exactly once:\n{text}"
    );
    assert!(
        text.contains("captured_cell captured"),
        "the genuine outer capture must be forwarded:\n{text}"
    );
    assert!(
        !text.contains("cell_new fresh") && text.contains("let fresh: unknown = 34"),
        "a closure-local assignment must not box a later outer homonym:\n{text}"
    );
    assert!(
        !text.contains("captured_cell fresh")
            && !text.contains("captured_cell n")
            && !text.contains("captured_cell local")
            && !text.contains("captured_cell countdown"),
        "parameters, locals, and a local function's self name are not captures:\n{text}"
    );
}

#[test]
fn match_pattern_bindings_are_scoped_out_of_closure_captures() {
    let source = r#"
fn main() {
    shadow = 99
    choose = () => {
        return match 1 {
            shadow if shadow == 1 => shadow
            _ => 0
        }
    }
    if (choose() != 1 || shadow != 99) {
        panic("match pattern binding escaped its arm scope")
    }
}
"#;

    check_source("match-capture.ku", source).expect("match capture should check");
    run_source("match-capture.ku", source).expect("match capture should run");

    let text = lower_ir(source);
    assert!(
        !text.contains("cell_new shadow") && !text.contains("captured_cell shadow"),
        "an arm-local pattern binding must not capture or box the outer homonym:\n{text}"
    );
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
    assert!(
        c.contains("typedef struct { size_t len; int64_t* data; size_t capacity; } KuArray_int;")
    );
    assert!(c.contains("ku_array_bounds_fail"));
    assert!(c.contains("ku_array_get_int"));
}

#[test]
fn ir_compound_assignment_evaluates_index_once() {
    let text = lower_ir(
        r#"
fn idx(): int {
    return 0
}

fn row(): int {
    return 0
}

fn col(): int {
    return 0
}

fn main() {
    nums:[int] = [1]
    nums[idx()] += 1
    rows:[[int]] = [[1]]
    rows[row()][col()] += 1
}
"#,
    );
    let idx_calls = text.matches("idx()").count();
    assert_eq!(
        idx_calls, 2,
        "compound assignment should lower one idx() call plus the function header:\n{text}"
    );
    assert_eq!(
        text.matches("row()").count(),
        2,
        "compound assignment should lower one row() call plus the function header:\n{text}"
    );
    assert_eq!(
        text.matches("col()").count(),
        2,
        "compound assignment should lower one col() call plus the function header:\n{text}"
    );
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
            is_closure_body: false,
            captures: Vec::new(),
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

#[test]
fn llvm_backend_lowers_inactive_safepoint_to_continue_and_validates_both_edges() {
    let valid = ir::IrProgram {
        functions: vec![ir::IrFunction {
            id: ir::FunctionId(0),
            name: "main".to_string(),
            params: Vec::new(),
            return_type: ir::IrType::Void,
            blocks: vec![
                ir::IrBlock {
                    id: ir::BlockId(0),
                    name: "entry".to_string(),
                    instructions: Vec::new(),
                    terminator: ir::IrTerminator::Safepoint {
                        continue_block: ir::BlockId(1),
                        timeout_block: ir::BlockId(2),
                    },
                },
                ir::IrBlock {
                    id: ir::BlockId(1),
                    name: "continue".to_string(),
                    instructions: Vec::new(),
                    terminator: ir::IrTerminator::Return(None),
                },
                ir::IrBlock {
                    id: ir::BlockId(2),
                    name: "timeout".to_string(),
                    instructions: Vec::new(),
                    terminator: ir::IrTerminator::Return(None),
                },
            ],
            is_closure_body: false,
            captures: Vec::new(),
        }],
        layouts: ir::IrLayoutTable {
            structs: Vec::new(),
            enums: Vec::new(),
        },
    };
    let llvm = backend::llvm::generate_llvm_ir(&valid).expect("lower inactive safepoint");
    assert!(
        llvm.contains("b0:\n  br i1 false, label %b2, label %b1\n"),
        "LLVM must preserve both safepoint CFG successors while taking the inactive continuation edge:\n{llvm}"
    );

    let missing_timeout = ir::IrProgram {
        functions: vec![ir::IrFunction {
            id: ir::FunctionId(0),
            name: "main".to_string(),
            params: Vec::new(),
            return_type: ir::IrType::Void,
            blocks: vec![
                ir::IrBlock {
                    id: ir::BlockId(0),
                    name: "entry".to_string(),
                    instructions: Vec::new(),
                    terminator: ir::IrTerminator::Safepoint {
                        continue_block: ir::BlockId(1),
                        timeout_block: ir::BlockId(99),
                    },
                },
                ir::IrBlock {
                    id: ir::BlockId(1),
                    name: "continue".to_string(),
                    instructions: Vec::new(),
                    terminator: ir::IrTerminator::Return(None),
                },
            ],
            is_closure_body: false,
            captures: Vec::new(),
        }],
        layouts: ir::IrLayoutTable {
            structs: Vec::new(),
            enums: Vec::new(),
        },
    };
    let error = backend::llvm::generate_llvm_ir(&missing_timeout)
        .expect_err("CFG validation must retain the timeout successor")
        .to_string();
    assert!(
        error.contains("branches to missing block 99"),
        "unexpected error: {error}"
    );
}

#[test]
fn c_backend_validates_both_safepoint_cfg_edges_before_emission() {
    fn program(continue_block: ir::BlockId, timeout_block: ir::BlockId) -> ir::IrProgram {
        ir::IrProgram {
            functions: vec![ir::IrFunction {
                id: ir::FunctionId(0),
                name: "main".to_string(),
                params: Vec::new(),
                return_type: ir::IrType::Void,
                blocks: vec![
                    ir::IrBlock {
                        id: ir::BlockId(0),
                        name: "entry".to_string(),
                        instructions: Vec::new(),
                        terminator: ir::IrTerminator::Safepoint {
                            continue_block,
                            timeout_block,
                        },
                    },
                    ir::IrBlock {
                        id: ir::BlockId(1),
                        name: "continue".to_string(),
                        instructions: Vec::new(),
                        terminator: ir::IrTerminator::Return(None),
                    },
                    ir::IrBlock {
                        id: ir::BlockId(2),
                        name: "timeout".to_string(),
                        instructions: Vec::new(),
                        terminator: ir::IrTerminator::Return(None),
                    },
                ],
                is_closure_body: false,
                captures: Vec::new(),
            }],
            layouts: ir::IrLayoutTable {
                structs: Vec::new(),
                enums: Vec::new(),
            },
        }
    }

    let c = backend::c::generate_c_source(&program(ir::BlockId(1), ir::BlockId(2)))
        .expect("lower valid C safepoint");
    assert!(
        c.contains("goto block2; } else goto block1;"),
        "C must lower both valid safepoint successors"
    );

    for (continue_block, timeout_block, missing) in [
        (ir::BlockId(99), ir::BlockId(2), 99),
        (ir::BlockId(1), ir::BlockId(100), 100),
    ] {
        let error = backend::c::generate_c_source(&program(continue_block, timeout_block))
            .expect_err("C CFG validation must reject either missing safepoint successor")
            .to_string();
        assert!(
            error.contains(&format!("branches to missing block {missing}")),
            "unexpected error: {error}"
        );
    }
}

#[test]
fn llvm_backend_keeps_ordinary_calls_working_with_inserted_safepoints() {
    let ir = lower_checked_ir(
        r#"
fn double(value:int): int {
    return value * 2
}

fn main() {
    value = 0
    while (value < 1) {
        value = double(3)
    }
    print(value)
}
"#,
    );
    assert!(
        ir.functions
            .iter()
            .flat_map(|function| &function.blocks)
            .any(|block| matches!(&block.terminator, ir::IrTerminator::Safepoint { .. })),
        "direct call lowering must exercise the safepoint path"
    );

    let llvm = backend::llvm::generate_llvm_ir(&ir).expect("generate LLVM after direct call");
    assert!(llvm.contains("call i64 @ku_fn0_double(i64 3)"));
    assert!(llvm.contains("br i1 false"));
}

#[test]
fn ir_for_backedge_contains_cooperative_safepoint() {
    let ir = lower_checked_ir(
        r#"
fn main() {
    for value in 3 {
    }
}
"#,
    );
    let function = ir
        .functions
        .iter()
        .find(|function| function.name == "main")
        .expect("main IR function");
    let (loop_block, body_block) = function
        .blocks
        .iter()
        .find_map(|block| match &block.terminator {
            ir::IrTerminator::ForEach { body_block, .. } => Some((block.id, *body_block)),
            _ => None,
        })
        .expect("for loop terminator");
    let body = function
        .blocks
        .iter()
        .find(|block| block.id == body_block)
        .expect("for body block");
    let continue_block = match &body.terminator {
        ir::IrTerminator::Safepoint { continue_block, .. } => *continue_block,
        ref other => panic!("for back-edge must poll before iterating again, got {other:?}"),
    };
    let continuation = function
        .blocks
        .iter()
        .find(|block| block.id == continue_block)
        .expect("safepoint continuation block");
    assert_eq!(
        continuation.terminator,
        ir::IrTerminator::Jump(loop_block),
        "the successful poll must resume the ForEach terminator"
    );
}
