use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use ku::{
    backend, checker::Checker, cli::check_source, cli::run_cli, cli::run_source, ir, lexer::Lexer,
    package, parser::Parser,
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

fn assert_ir_cfg_acyclic(program: &ir::IrProgram) {
    fn visit(
        id: ir::BlockId,
        edges: &HashMap<ir::BlockId, Vec<ir::BlockId>>,
        visiting: &mut HashSet<ir::BlockId>,
        visited: &mut HashSet<ir::BlockId>,
    ) {
        if visited.contains(&id) {
            return;
        }
        assert!(
            visiting.insert(id),
            "IR CFG contains a cycle at block{}",
            id.0
        );
        for target in edges.get(&id).into_iter().flatten() {
            visit(*target, edges, visiting, visited);
        }
        visiting.remove(&id);
        visited.insert(id);
    }

    for function in &program.functions {
        let mut edges = HashMap::new();
        let block_ids = function
            .blocks
            .iter()
            .map(|block| block.id)
            .collect::<HashSet<_>>();
        assert_eq!(
            block_ids.len(),
            function.blocks.len(),
            "IR function '{}' contains duplicate block ids",
            function.name
        );
        for block in &function.blocks {
            let targets = match &block.terminator {
                ir::IrTerminator::Jump(target) => vec![*target],
                ir::IrTerminator::Branch {
                    then_block,
                    else_block,
                    ..
                } => vec![*then_block, *else_block],
                ir::IrTerminator::ForEach {
                    body_block,
                    after_block,
                    ..
                } => vec![*body_block, *after_block],
                ir::IrTerminator::ResultBranch {
                    ok_block,
                    err_block,
                    ..
                } => vec![*ok_block, *err_block],
                ir::IrTerminator::JumpErr { target, .. } => vec![*target],
                ir::IrTerminator::Next
                | ir::IrTerminator::PropagateErr(_)
                | ir::IrTerminator::Return(_)
                | ir::IrTerminator::Unreachable => Vec::new(),
            };
            for target in &targets {
                assert!(
                    block_ids.contains(target),
                    "IR block{} targets missing block{}",
                    block.id.0,
                    target.0
                );
            }
            edges.insert(block.id, targets);
        }
        let mut visiting = HashSet::new();
        let mut visited = HashSet::new();
        for block in &function.blocks {
            visit(block.id, &edges, &mut visiting, &mut visited);
        }
    }
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
fn ir_lowers_fail_inside_try_to_error_handler() {
    let text = lower_ir(
        r#"
fn main(): int! {
    try {
        fail "bad"
    } catch (err) {
        return ok(1)
    } finally {
        print("cleanup")
    }
    return ok(2)
}
"#,
    );

    assert!(text.contains("jump_err"), "unexpected IR:\n{text}");
    assert!(
        text.contains("bind_error err from"),
        "unexpected IR:\n{text}"
    );
    assert!(!text.contains("fail \"bad\""), "unexpected IR:\n{text}");
}

#[test]
fn ir_optimizer_folds_constants_and_removes_dead_branch_blocks() {
    let tokens = Lexer::new(
        r#"
fn main() {
    value = 1 + 2
    if (true) {
        print(value)
    } else {
        print(0)
    }
}
"#,
    )
    .tokenize()
    .expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new().check(&program).expect("check");
    let lowered = ir::lower_program(&program).expect("lower ir");
    let optimized = ir::optimize_program(&lowered);
    assert_ir_cfg_acyclic(&optimized);
    let text = optimized.to_string();
    assert!(
        text.contains("%t0: int = 3"),
        "constant folding should fold simple int arithmetic:\n{text}"
    );
    assert!(
        text.contains("jump block"),
        "constant branch should become a jump:\n{text}"
    );
    assert!(
        !text.contains("else:"),
        "unreachable else block should be removed:\n{text}"
    );
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

    assert!(c.contains("void ku_main("));
    assert!(c.contains("int main(void)"));
    assert!(c.contains("if ("));
    assert!(c.contains("goto block"));
    assert!(c.contains("block"));
    assert!(c.contains("return total;"));
}

#[test]
fn checker_does_not_stack_overflow_on_untyped_local_recursion() {
    let source = r#"
fn main() {
    fn forever() {
        forever()
    }
}
"#;

    check_source("inline.ku", source).expect("recursive local inference should be bounded");
}

#[test]
fn native_build_rejects_async_syntax_with_clear_error() {
    let dir = unique_temp_path("native-async");
    fs::create_dir_all(&dir).expect("create temp dir");
    let file = dir.join("main.ku");
    fs::write(
        &file,
        r#"
async fn main() {
    print("hi")
}
"#,
    )
    .expect("write source");

    let err = run_cli(vec![
        "ku".to_string(),
        "build".to_string(),
        "--native".to_string(),
        file.display().to_string(),
    ])
    .expect_err("native async should be rejected")
    .to_string();
    assert!(
        err.contains("native C prototype does not support async/await yet"),
        "unexpected error: {err}"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_build_rejects_object_destructuring_until_lowered() {
    let dir = unique_temp_path("native-object-destructure");
    fs::create_dir_all(&dir).expect("create temp dir");
    let file = dir.join("main.ku");
    fs::write(
        &file,
        r#"
fn main() {
    user = { name: "Ku" }
    { name } = user
    print(name)
}
"#,
    )
    .expect("write source");

    let err = run_cli(vec![
        "ku".to_string(),
        "build".to_string(),
        "--emit-ir".to_string(),
        file.display().to_string(),
    ])
    .expect_err("IR/native object destructuring should be rejected")
    .to_string();
    assert!(
        err.contains("IR/native lowering does not support object destructuring yet"),
        "unexpected error: {err}"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_c_rejects_string_concat_until_kustring_abi_is_lowered() {
    let tokens = Lexer::new(
        r#"
fn main() {
    name = "Ku"
    text = "Hello " + name
    print(text)
}
"#,
    )
    .tokenize()
    .expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new().check(&program).expect("check");
    let ir = ir::lower_program(&program).expect("lower");
    let err = backend::c::generate_c_source(&ir)
        .expect_err("native C string concat must be rejected until KuString exists")
        .to_string();
    assert!(
        err.contains("string concatenation") && err.contains("KuString ABI"),
        "unexpected error: {err}"
    );
}

#[test]
fn native_build_ignores_async_words_in_comments_strings_and_identifiers() {
    let dir = unique_temp_path("native-async-trivia");
    fs::create_dir_all(&dir).expect("create temp dir");
    let file = dir.join("main.ku");
    fs::write(
        &file,
        r#"
/* async await in block comment */
fn main() {
    async_value = 1
    print("async await in string")
    print(`await in template`)
    print(async_value)
}
"#,
    )
    .expect("write source");

    run_cli(vec![
        "ku".to_string(),
        "build".to_string(),
        "--native".to_string(),
        file.display().to_string(),
    ])
    .expect("native build should ignore async words in trivia");
    fs::remove_file(file.with_extension("c")).ok();
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_c_backend_lowers_result_int_question_and_propagation() {
    let tokens = Lexer::new(
        r#"
fn value(): int! {
    return ok(7)
}

fn main(): int! {
    item = value()?
    return ok(item + 1)
}
"#,
    )
    .tokenize()
    .expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new().check(&program).expect("check");
    let ir = ir::lower_program(&program).expect("lower");
    let c = backend::c::generate_c_source(&ir).expect("generate c");

    assert!(c.contains("KuResult_int ku_main("));
    assert!(c.contains("int main(void)"));
    assert!(c.contains("typedef struct { bool ok; int64_t value; KuError error; } KuResult_int"));
    assert!(c.contains("if (t0.ok) goto block"));
    assert!(c.contains("ku_result_take_int(&t0)"));
    assert!(c.contains("int64_t item = "));
    assert!(c.contains("ku_result_move_int(&t"));
}

#[test]
fn native_c_backend_lowers_owned_array_result_payloads() {
    let tokens = Lexer::new(
        r#"
fn values(): [int]! {
    return ok([1, 2])
}

fn main() {
    print("ok")
}
"#,
    )
    .tokenize()
    .expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new().check(&program).expect("check");
    let ir = ir::lower_program(&program).expect("lower");
    let c = backend::c::generate_c_source(&ir).expect("generate array Result C");
    assert!(c.contains(
        "typedef struct { bool ok; KuArray_int value; KuError error; } KuResult_array_int"
    ));
    assert!(c.contains("ku_result_move_array_int"));
    assert!(c.contains("ku_result_take_array_int"));
    assert!(c.contains("ku_result_drop_array_int"));
    assert!(c.contains("ku_array_move_int"));
}

#[test]
fn native_try_finally_routes_return_and_error_through_cleanup_blocks() {
    let tokens = Lexer::new(
        r#"
fn value(flag:bool): int! {
    try {
        if (flag) {
            return ok(7)
        }
        fail "bad"
    } catch (err) {
        print(err.message)
        return ok(8)
    } finally {
        print("cleanup")
    }
    return ok(9)
}

fn main(): int! {
    return value(true)
}
"#,
    )
    .tokenize()
    .expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new().check(&program).expect("check");
    let ir = ir::lower_program(&program).expect("lower");
    let text = ir.to_string();
    assert!(text.contains("finally_return"), "unexpected IR:\n{text}");
    assert!(text.contains("__ku_return_"), "unexpected IR:\n{text}");
    assert!(text.contains("__ku_error_"), "unexpected IR:\n{text}");

    let c = backend::c::generate_c_source(&ir).expect("generate try/finally C");
    assert!(c.contains("typedef struct KuError"));
    assert!(c.contains("KuError err = "));
    assert!(c.contains(".error;"));
    assert!(c.contains("goto block"));
    assert!(c.contains("ku_result_move_int"));
}

#[test]
fn native_c_backend_lowers_bounded_array_reads_and_writes() {
    let tokens = Lexer::new(
        r#"
fn replace(values:[int]): int {
    values[0] = 8
    return values[0]
}

fn main() {
    values = [1, 2, 3]
    values[1] = 7
    copy = values.clone()
    copy[0] = 9
    if (values[1] != 7) {
        panic("bad array write")
    }
    if (values[0] != 1) {
        panic("array assignment must copy")
    }
    changed = replace(values.clone())
    if (changed != 8 || values[0] != 1) {
        panic("array argument must copy")
    }
    print(values[0])
}
"#,
    )
    .tokenize()
    .expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new().check(&program).expect("check");
    let ir = ir::lower_program(&program).expect("lower");
    let c = backend::c::generate_c_source(&ir).expect("generate c");

    assert!(c.contains("typedef struct { size_t len; int64_t* data; } KuArray_int"));
    assert!(c.contains("ku_array_make_int(3"));
    assert!(c.contains("ku_array_clone_int(values)"));
    assert!(c.contains("replace(ku_array_move_int(&t"));
    assert!(c.contains("ku_array_drop_int(&values)"));
    assert!(c.contains("ku_array_move_int(&"));
    assert!(c.contains("index < 0 || (uint64_t)index >= array.len"));
    assert!(c.contains("index < 0 || (uint64_t)index >= array->len"));
    assert!(c.contains("ku_array_get_int("));
    assert!(c.contains("*ku_array_at_int("));
}

#[test]
fn checker_enforces_owned_move_and_explicit_clone() {
    let moved = check_err(
        r#"
fn take(values:[int]): null {
    return null
}

fn main() {
    values = [1, 2]
    take(values)
    print(values[0])
}
"#,
    );
    assert!(
        moved.contains("use of moved value 'values'"),
        "unexpected move diagnostic: {moved}"
    );

    let branch = check_err(
        r#"
fn take(values:[int]): null {
    return null
}

fn main() {
    values = [1, 2]
    if (true) {
        take(values)
    }
    print(values[0])
}
"#,
    );
    assert!(
        branch.contains("use of moved value 'values'"),
        "branch move must conservatively merge: {branch}"
    );

    let cloned = r#"
fn take(values:[int]): null {
    return null
}

fn main() {
    values = [1, 2]
    take(values.clone())
    print(values[0])
}
"#;
    let tokens = Lexer::new(cloned).tokenize().expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new()
        .check(&program)
        .expect("explicit clone should check");

    let object_destructure_move = check_err(
        r#"
fn main() {
    user = { name: "Ku" }
    { name } = user
    print(user.name)
}
"#,
    );
    assert!(
        object_destructure_move.contains("use of moved value 'user'"),
        "object destructuring must consume the source object: {object_destructure_move}"
    );

    let object_destructure_reinit = r#"
fn take(value: str): null {
    return null
}

fn main() {
    name = "old"
    take(name)
    user = { name: "Ku" }
    { name } = user
    print(name)
}
"#;
    let tokens = Lexer::new(object_destructure_reinit)
        .tokenize()
        .expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new()
        .check(&program)
        .expect("object destructuring assignment should reinitialize moved locals");
    run_source("inline.ku", object_destructure_reinit)
        .expect("object destructuring reinitialization should run");

    let loop_move = check_err(
        r#"
fn take(values:[int]): null {
    return null
}

fn main() {
    values = [1, 2]
    while (true) {
        take(values)
        continue
    }
}
"#,
    );
    assert!(
        loop_move.contains("later iteration would reuse a moved value"),
        "loop-carried move must be rejected: {loop_move}"
    );

    let break_move = r#"
fn take(values:[int]): null {
    return null
}

fn main() {
    values = [1, 2]
    while (true) {
        take(values)
        break
    }
}
"#;
    let tokens = Lexer::new(break_move).tokenize().expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new()
        .check(&program)
        .expect("a move followed by unconditional break has no loop backedge");

    let repeated_question = check_err(
        r#"
fn values(): [int]! {
    return ok([1])
}

fn main(): null! {
    result = values()
    first = result?
    second = result?
    print(first[0] + second[0])
    return ok(null)
}
"#,
    );
    assert!(
        repeated_question.contains("use of moved value 'result'"),
        "Result '?' must consume an owned Result once: {repeated_question}"
    );

    let match_move = check_err(
        r#"
fn main() {
    values = [1]
    selected = match true {
        true => values
        _ => [2]
    }
    print(selected[0])
    print(values[0])
}
"#,
    );
    assert!(
        match_move.contains("use of moved value 'values'"),
        "match result arms must merge owned moves: {match_move}"
    );
}

#[test]
fn native_owned_assignment_swap_nested_clone_and_cross_result_error_are_safe() {
    let source = r#"
fn load(): [int]! {
    fail "bad"
}

fn convert(): null! {
    load()?
    return ok(null)
}

fn main(): null! {
    left = [1]
    right = [2]
    left, right = right, left
    left = left
    nested = [[1], [2]]
    copy = nested.clone()
    print(left[0] + right[0] + copy[0][0])
    return convert()
}
"#;
    let tokens = Lexer::new(source).tokenize().expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new().check(&program).expect("check");
    let ir = ir::lower_program(&program).expect("lower");
    let c = backend::c::generate_c_source(&ir).expect("generate c");

    assert!(c.contains("{ KuArray_int __ku_store = ku_array_move_int(&left);"));
    assert!(c.contains("ku_array_clone_array_int"));
    assert!(c.contains("ku_array_clone_int(array.data[index])"));
    assert!(c.contains("ku_array_drop_int(&array->data[index])"));
    assert!(c.contains("return (KuResult_null){ false, 0, __ku_error };"));
    assert!(!c.contains("KuResult_null __ku_error_return = ku_result_move_array_int"));
    assert!(c.contains("ku_result_drop_null(&result)"));
}

#[test]
fn std_task_exposes_bounded_runtime_stats_and_stress_report() {
    let source = r#"
import "std.task"
import "std.time"

fn main() {
    started = time.millis()
    before = task.stats()
    report = task.stress(2000, 4, 1)
    after = task.stats()
    print(report.demand)
    print(report.peak_active)
    print(report.accepted + report.rejected_limit + report.rejected_queue + report.rejected_internal)
    print(after.finished_tasks - before.finished_tasks)
    print(time.millis() - started)
}
"#;
    check_source("inline.ku", source).expect("task stress API should check");
    run_source("inline.ku", source).expect("task stress API should run and drain");

    let err = check_source(
        "inline.ku",
        r#"
import "std.task"
fn main() {
    task.stress("many", 4, 1)
}
"#,
    )
    .expect_err("task.stress must reject non-int demand")
    .to_string();
    assert!(
        err.contains("expected int"),
        "task.stress argument types must be checked: {err}"
    );
}

#[test]
fn async_task_handles_are_move_only_and_await_once() {
    let repeated = check_err(
        r#"
async fn load(): int! {
    return ok(1)
}

async fn main(): null! {
    task = load()
    first = await task?
    second = await task?
    println(first + second)
    return ok(null)
}
"#,
    );
    assert!(
        repeated.contains("task 'task' has already been awaited"),
        "task await should be single-use: {repeated}"
    );

    let cloned = check_err(
        r#"
async fn load(): int! {
    return ok(1)
}

async fn main(): null! {
    task = load()
    copy = task.clone()
    value = await copy?
    println(value)
    return ok(null)
}
"#,
    );
    assert!(
        cloned.contains("task values cannot be cloned"),
        "task handles must be move-only: {cloned}"
    );

    let array_clone = check_err(
        r#"
async fn load(): int! {
    return ok(1)
}

async fn main(): null! {
    tasks = [load()]
    copy = tasks.clone()
    value = await copy[0]?
    println(value)
    return ok(null)
}
"#,
    );
    assert!(
        array_clone.contains("task values cannot be cloned"),
        "collections containing task handles must not clone: {array_clone}"
    );

    let method = check_err(
        r#"
async fn load(): int! {
    return ok(1)
}

async fn main(): null! {
    task = load()
    println(task.status())
    return ok(null)
}
"#,
    );
    assert!(
        method.contains("task handles can only be awaited"),
        "task lifecycle methods should not be part of the user API: {method}"
    );
}

#[test]
fn std_root_import_allows_lowercase_task_and_time_modules() {
    let source = r#"
import { task, time } from "std"

fn main() {
    now = time.millis()
    stats = task.stats()
    print(now >= 0)
    print(stats.active_tasks >= 0)
}
"#;
    check_source("inline.ku", source).expect("lowercase std root imports should check");
    run_source("inline.ku", source).expect("lowercase std root imports should run");
}

#[test]
fn native_c_backend_lowers_enum_payload_and_guarded_match_cfg() {
    let source = r#"
enum Maybe {
    Some(value:int)
    None
}

fn main() {
    value = Maybe.Some(7)
    result = match value {
        Maybe.Some(n) if (n > 0) => n + 1
        Maybe.Some(_) => 0
        Maybe.None => -1
    }
    print(result)
}
"#;
    let tokens = Lexer::new(source).tokenize().expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new().check(&program).expect("check");
    let ir = ir::lower_program(&program).expect("lower");
    let ir_text = ir.to_string();
    assert_ir_cfg_acyclic(&ir);
    let c = backend::c::generate_c_source(&ir).expect("generate c");

    assert!(ir_text.contains("match_arm"), "unexpected IR:\n{ir_text}");
    assert!(ir_text.contains("match_next"), "unexpected IR:\n{ir_text}");
    assert!(ir_text.contains("match_after"), "unexpected IR:\n{ir_text}");
    assert!(c.contains("typedef struct KuEnum_Maybe"));
    assert!(c.contains("int32_t tag;"));
    assert!(c.contains("payload.Some.value"));
    assert!(c.contains(".tag = 0"));
    assert!(c.contains("goto block"));
    assert!(!c.contains("while (1)"), "match lowering must not retry");
}

#[test]
fn native_c_backend_lowers_nested_enum_match_payloads() {
    let tokens = Lexer::new(
        r#"
enum Inner {
    Number(value:int)
    Empty
}

enum Expr {
    Box(value:Inner)
    Other
}

fn main() {
    value = Expr.Box(Inner.Number(9))
    result = match value {
        Expr.Box(Inner.Number(n)) => n
        Expr.Box(_) => 0
        Expr.Other => -1
    }
    print(result)
}
"#,
    )
    .tokenize()
    .expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new().check(&program).expect("check");
    let ir = ir::lower_program(&program).expect("lower");
    let c = backend::c::generate_c_source(&ir).expect("generate c");

    assert!(c.contains("KuEnum_Inner value;"));
    assert!(c.contains("payload.Box.value"));
    assert!(c.contains("payload.Number.value"));
}

#[test]
fn guarded_wildcard_does_not_make_later_match_arms_unreachable() {
    let source = r#"
enum State {
    Ready
    Done
}

fn main() {
    state = State.Done
    label = match state {
        _ if (false) => "guarded"
        State.Ready => "ready"
        State.Done => "done"
    }
    print(label)
}
"#;

    let tokens = Lexer::new(source).tokenize().expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new().check(&program).expect("check");
}

#[test]
fn guarded_wildcard_alone_is_not_exhaustive_for_enum_match() {
    let err = check_err(
        r#"
enum State {
    Ready
    Done
}

fn main() {
    state = State.Done
    label = match state {
        _ if (true) => "guarded"
    }
    print(label)
}
"#,
    );

    assert!(err.contains("not exhaustive"), "unexpected error: {err}");
}

#[test]
fn duplicate_unguarded_literal_match_arm_is_unreachable() {
    let err = check_err(
        r#"
fn main() {
    value = 1
    text = match value {
        1 => "one"
        1 => "again"
        _ => "other"
    }
    print(text)
}
"#,
    );

    assert!(err.contains("unreachable"), "unexpected error: {err}");
}

#[test]
fn match_guarded_variant_then_unguarded_variant_is_allowed() {
    let source = r#"
enum State {
    Ready
    Done
}

fn main() {
    state = State.Ready
    label = match state {
        State.Ready if (false) => "guarded"
        State.Ready => "ready"
        State.Done => "done"
    }
    print(label)
}
"#;

    let tokens = Lexer::new(source).tokenize().expect("lex");
    let program = Parser::new(tokens).parse().expect("parse");
    Checker::new().check(&program).expect("check");
}

#[test]
fn match_unguarded_variant_then_guarded_variant_is_unreachable() {
    let err = check_err(
        r#"
enum State {
    Ready
    Done
}

fn main() {
    state = State.Ready
    label = match state {
        State.Ready => "ready"
        State.Ready if (true) => "again"
        State.Done => "done"
    }
    print(label)
}
"#,
    );

    assert!(err.contains("unreachable"), "unexpected error: {err}");
}

#[test]
fn nested_enum_match_patterns_bind_and_run_with_guards() {
    let source = r#"
enum Inner {
    Number(value:int)
    Empty
}

enum Expr {
    Box(value:Inner)
    Other
}

fn main() {
    value = Expr.Box(Inner.Number(7))
    text = match value {
        Expr.Box(Inner.Number(n)) if (n == 7) => "seven"
        Expr.Box(_) => "box"
        Expr.Other => "other"
    }
    if (text != "seven") {
        panic("bad nested match")
    }
}
"#;

    run_source("inline.ku", source).expect("nested enum match should run");
}

#[test]
fn nested_enum_match_partial_payload_is_not_exhaustive() {
    let err = check_err(
        r#"
enum Inner {
    Number(value:int)
    Empty
}

enum Expr {
    Box(value:Inner)
    Other
}

fn main() {
    value = Expr.Box(Inner.Empty)
    text = match value {
        Expr.Box(Inner.Number(n)) => "boxed number"
        Expr.Other => "other"
    }
    print(text)
}
"#,
    );

    assert!(err.contains("not exhaustive"), "unexpected error: {err}");
    assert!(err.contains("Box"), "unexpected error: {err}");
}

#[test]
fn duplicate_nested_match_pattern_is_unreachable() {
    let err = check_err(
        r#"
enum Inner {
    Number(value:int)
    Empty
}

enum Expr {
    Box(value:Inner)
    Other
}

fn main() {
    value = Expr.Box(Inner.Number(1))
    text = match value {
        Expr.Box(Inner.Number(1)) => "one"
        Expr.Box(Inner.Number(1)) => "again"
        Expr.Box(_) => "box"
        Expr.Other => "other"
    }
    print(text)
}
"#,
    );

    assert!(err.contains("unreachable"), "unexpected error: {err}");
}

#[test]
fn http_stdlib_requires_explicit_std_import() {
    let err = check_err(
        r#"
fn main() {
    result = http.get("http://example.com")
    print(str(result))
}
"#,
    );

    assert!(
        err.contains("std module 'http' must be imported"),
        "unexpected error: {err}"
    );
}

#[test]
fn fs_stdlib_requires_explicit_std_import() {
    let err = check_source(
        "inline.ku",
        r#"
fn main() {
    text = fs.try_read("missing.txt")
    print(str(text))
}
"#,
    )
    .expect_err("fs should require explicit std import")
    .to_string();

    assert!(
        err.contains("std module 'fs' must be imported"),
        "unexpected error: {err}"
    );

    let source = r#"
import "std.fs"

fn load(): str! {
    return fs.try_read("definitely-missing-ku-file.txt")
}

fn main() {
    try {
        value = load()?
        print(value)
    } catch (err) {
        print("missing")
    }
}
"#;

    check_source("inline.ku", source).expect("std.fs import should check");
    run_source("inline.ku", source).expect("std.fs import should run");
}

#[test]
fn imported_http_stdlib_returns_recoverable_errors_without_network_retry() {
    let dir = unique_temp_path("std-http");
    fs::create_dir_all(&dir).expect("create temp dir");
    let main = dir.join("main.ku");
    let source = r#"
import "std.http"

fn main(): null! {
    res = http.get("ftp://example.com")?
    print(res.body)
    return ok(null)
}
"#;
    fs::write(&main, source).expect("write main");

    check_source(&main.display().to_string(), source).expect("std.http import should check");
    let err = run_source(&main.display().to_string(), source)
        .expect_err("invalid url should be a recoverable http error")
        .to_string();
    assert!(
        err.contains("http url must start with http:// or https://"),
        "unexpected error: {err}"
    );
    fs::remove_dir_all(dir).expect("remove temp dir");
}

#[test]
fn old_http_try_get_and_std_colon_import_are_rejected() {
    let err = check_source(
        "inline.ku",
        r#"
import "std.http"
fn main() {
    http.try_get("http://example.com")
}
"#,
    )
    .expect_err("http.try_get should not exist")
    .to_string();
    if !err.contains("unknown stdlib function") && !err.contains("undefined variable 'http'") {
        let err = check_source(
            "inline.ku",
            r#"
import http from "std.http"
fn main() {
    http.try_get("http://example.com")
}
"#,
        )
        .expect_err("http.try_get should not exist")
        .to_string();
        assert!(
            err.contains("unknown stdlib function") || err.contains("undefined variable 'http'"),
            "unexpected error: {err}"
        );
    }

    let err = check_source(
        "inline.ku",
        r#"
import "std:http"
fn main() {
    print("bad")
}
"#,
    )
    .expect_err("std: import should not be supported")
    .to_string();
    assert!(
        err.contains("unsupported import path") || err.contains("failed to resolve"),
        "unexpected error: {err}"
    );
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

#[test]
fn package_file_dependency_is_cached_and_importable() {
    let dir = unique_temp_path("package-remote-dep");
    let app_src = dir.join("app").join("src");
    let dep_src = dir.join("registry").join("util").join("src");
    fs::create_dir_all(&app_src).expect("create app src");
    fs::create_dir_all(&dep_src).expect("create dep src");
    fs::write(dep_src.join("util.ku"), "fn Value(): int { return 42 }").expect("write dep util");
    let dep_root = dir.join("registry").join("util");
    let checksum = package::package_source_checksum(&dep_root).expect("checksum");
    fs::write(
        dir.join("app").join("ku.mod"),
        format!(
            r#"
name = "demo_pkg"
version = "0.1.4"
dep.util = "1.0.0"
dep.util.source = "file://{}"
dep.util.checksum = "{}"
"#,
            dep_root.to_string_lossy().replace('\\', "/"),
            checksum
        ),
    )
    .expect("write ku.mod");
    let main = app_src.join("main.ku");
    fs::write(
        &main,
        r#"
import { Value } from "@util/util"
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
    let lock = fs::read_to_string(dir.join("app").join("ku.lock")).expect("read lock");
    assert!(
        lock.contains("[[package_dependency]]"),
        "unexpected lock:\n{lock}"
    );
    assert!(lock.contains("name = \"util\""), "unexpected lock:\n{lock}");
    assert!(lock.contains(&checksum), "unexpected lock:\n{lock}");
    assert!(
        dir.join("app")
            .join(".ku")
            .join("cache")
            .join("packages")
            .join("util")
            .join("1.0.0")
            .join("src")
            .join("util.ku")
            .exists(),
        "dependency should be cached"
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn package_dependency_without_source_fails_closed_even_when_cache_exists() {
    let dir = unique_temp_path("package-dep-nosource");
    let app_src = dir.join("app").join("src");
    let cache_src = dir
        .join("app")
        .join(".ku")
        .join("cache")
        .join("packages")
        .join("util")
        .join("1.0.0")
        .join("src");
    fs::create_dir_all(&app_src).expect("create app src");
    fs::create_dir_all(&cache_src).expect("create stale cache src");
    fs::write(cache_src.join("util.ku"), "fn Value(): int { return 42 }")
        .expect("write stale cached dep");
    fs::write(
        dir.join("app").join("ku.mod"),
        r#"
name = "demo_pkg"
dep.util = "1.0.0"
"#,
    )
    .expect("write ku.mod");
    let main = app_src.join("main.ku");
    fs::write(
        &main,
        r#"
import { Value } from "@util/util"
fn main() { print(Value()) }
"#,
    )
    .expect("write main");

    let err = run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect_err("dependency without trusted source should fail")
    .to_string();
    assert!(
        err.contains("trusted source") || err.contains("registry_trust_unconfigured"),
        "unexpected error: {err}"
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn package_dependency_import_rejects_parent_escape() {
    let dir = unique_temp_path("package-dep-escape");
    let app_src = dir.join("app").join("src");
    let dep_src = dir.join("registry").join("util").join("src");
    fs::create_dir_all(&app_src).expect("create app src");
    fs::create_dir_all(&dep_src).expect("create dep src");
    fs::write(dep_src.join("util.ku"), "fn Value(): int { return 42 }").expect("write dep util");
    let dep_root = dir.join("registry").join("util");
    fs::write(
        dir.join("app").join("ku.mod"),
        format!(
            r#"
name = "demo_pkg"
dep.util = "1.0.0"
dep.util.source = "file://{}"
"#,
            dep_root.to_string_lossy().replace('\\', "/")
        ),
    )
    .expect("write ku.mod");
    let main = app_src.join("main.ku");
    fs::write(
        &main,
        r#"
import { Value } from "@util/../secret"
fn main() { print(Value()) }
"#,
    )
    .expect("write main");

    let err = run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect_err("dependency escape should fail")
    .to_string();
    assert!(err.contains("dependency root"), "unexpected error: {err}");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn package_file_url_accepts_triple_slash_windows_path() {
    let dir = unique_temp_path("package-file-url-triple");
    let app_src = dir.join("app").join("src");
    let dep_src = dir.join("registry").join("util").join("src");
    fs::create_dir_all(&app_src).expect("create app src");
    fs::create_dir_all(&dep_src).expect("create dep src");
    fs::write(dep_src.join("util.ku"), "fn Value(): int { return 7 }").expect("write dep util");
    let dep_root = dir.join("registry").join("util");
    fs::write(
        dir.join("app").join("ku.mod"),
        format!(
            r#"
name = "demo_pkg"
dep.util = "1.0.0"
dep.util.source = "file:///{}"
"#,
            dep_root.to_string_lossy().replace('\\', "/")
        ),
    )
    .expect("write ku.mod");
    let main = app_src.join("main.ku");
    fs::write(
        &main,
        r#"
import { Value } from "@util/util"
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

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn package_file_dependency_without_checksum_refreshes_changed_cache() {
    let dir = unique_temp_path("package-dep-refresh");
    let app_src = dir.join("app").join("src");
    let dep_src = dir.join("registry").join("util").join("src");
    fs::create_dir_all(&app_src).expect("create app src");
    fs::create_dir_all(&dep_src).expect("create dep src");
    let dep_file = dep_src.join("util.ku");
    fs::write(&dep_file, "fn Value(): int { return 1 }").expect("write dep util");
    let dep_root = dir.join("registry").join("util");
    fs::write(
        dir.join("app").join("ku.mod"),
        format!(
            r#"
name = "demo_pkg"
dep.util = "1.0.0"
dep.util.source = "file://{}"
"#,
            dep_root.to_string_lossy().replace('\\', "/")
        ),
    )
    .expect("write ku.mod");
    let main = app_src.join("main.ku");
    fs::write(
        &main,
        r#"
import { Value } from "@util/util"
fn main() { print(Value()) }
"#,
    )
    .expect("write main");

    run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect("first package check");
    fs::write(&dep_file, "fn Value(): int { return 2 }").expect("update dep util");
    run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect("second package check");
    let cached = fs::read_to_string(
        dir.join("app")
            .join(".ku")
            .join("cache")
            .join("packages")
            .join("util")
            .join("1.0.0")
            .join("src")
            .join("util.ku"),
    )
    .expect("read cached util");
    assert!(cached.contains("return 2"), "unexpected cache:\n{cached}");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn package_manifest_rejects_bad_checksum_format() {
    let dir = unique_temp_path("package-bad-checksum-format");
    let app_src = dir.join("app").join("src");
    fs::create_dir_all(&app_src).expect("create app src");
    fs::write(app_src.join("main.ku"), "fn main() { print(\"ok\") }").expect("write main");
    fs::write(
        dir.join("app").join("ku.mod"),
        r#"
name = "demo_pkg"
dep.util = "1.0.0"
dep.util.checksum = "bad"
"#,
    )
    .expect("write ku.mod");

    let err = package::discover_for_file(&app_src.join("main.ku"))
        .expect_err("bad checksum should fail")
        .to_string();
    assert!(err.contains("checksum"), "unexpected error: {err}");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn package_errors_carry_domain_and_code_metadata() {
    let err = package::parse_manifest(
        r#"
name = "demo"
version = "bad"
"#,
        Default::default(),
    )
    .expect_err("bad version should fail");

    assert_eq!(err.domain.as_deref(), Some("package"));
    assert_eq!(err.code.as_deref(), Some("invalid_version"));

    let err = package::parse_manifest(
        r#"
name = "demo"
dep.bad = "1.0.0"
dep.bad.checksum = "bad"
"#,
        Default::default(),
    )
    .expect_err("bad checksum should fail");

    assert_eq!(err.domain.as_deref(), Some("package"));
    assert_eq!(err.code.as_deref(), Some("invalid_checksum"));
}

#[test]
fn registry_manifest_and_lock_schema_parse_strictly() {
    let checksum = format!("sha256-{}", "a".repeat(64));
    let manifest = package::parse_registry_manifest(
        &format!(
            r#"
name = "math"
version = "0.1.0"
source = "https://registry.example/ku/math/0.1.0.tar.zst"
checksum = "{checksum}"
"#
        ),
        Default::default(),
    )
    .expect("registry manifest");
    assert_eq!(manifest.name, "math");
    assert_eq!(manifest.version, "0.1.0");

    let packages = package::parse_registry_lock(
        &format!(
            r#"
[[package]]
name = "math"
version = "0.1.0"
source = "registry"
url = "https://registry.example/ku/math/0.1.0.tar.zst"
checksum = "{checksum}"
cache_key = "math-0.1.0-sha256-{}"
"#,
            "a".repeat(64)
        ),
        Default::default(),
    )
    .expect("registry lock");
    assert_eq!(packages.len(), 1);
    assert_eq!(packages[0].source, "registry");
}

#[test]
fn registry_schema_rejects_missing_bad_semver_and_checksum() {
    let err = package::parse_registry_manifest(
        r#"
name = "math"
version = "latest"
source = "https://registry.example/math.tar.zst"
checksum = "bad"
"#,
        Default::default(),
    )
    .expect_err("bad semver should fail");
    assert_eq!(err.code.as_deref(), Some("invalid_version"));

    let err = package::parse_registry_manifest(
        r#"
name = "math"
version = "0.1.0"
source = "https://registry.example/math.tar.zst"
"#,
        Default::default(),
    )
    .expect_err("missing checksum should fail");
    assert_eq!(err.code.as_deref(), Some("missing_registry_field"));

    let err = package::parse_registry_lock(
        r#"
[[package]]
name = "math"
version = "0.1.0"
source = "registry"
url = "https://registry.example/math.tar.zst"
checksum = "sha256-deadbeef"
cache_key = "math-0.1.0"
"#,
        Default::default(),
    )
    .expect_err("short sha256 should fail");
    assert_eq!(err.code.as_deref(), Some("invalid_registry_checksum"));
}

#[test]
fn registry_index_ed25519_verifier_accepts_signed_bytes_and_rejects_tampering() {
    use ed25519_dalek::{Signer, SigningKey};
    use ku::package::RegistryIndexVerifier;

    let index_url = "https://registry.example/ku/index";
    let checksum = format!("sha256-{}", "a".repeat(64));
    let source = format!(
        "name = \"math\"\nversion = \"0.1.0\"\nsource = \"https://registry.example/ku/math/0.1.0.tar.zst\"\nchecksum = \"{checksum}\"\n"
    );
    let signing_key = SigningKey::from_bytes(&[7u8; 32]);
    let signature = signing_key.sign(source.as_bytes()).to_bytes();
    let verifier = package::Ed25519RegistryIndexVerifier::new(
        signing_key.verifying_key().to_bytes(),
        signature,
    );

    verifier
        .verify(index_url, source.as_bytes(), Default::default())
        .expect("signed registry index should verify");

    let err = verifier
        .verify(
            index_url,
            source.replace("0.1.0", "0.1.1").as_bytes(),
            Default::default(),
        )
        .expect_err("tampered registry index should fail")
        .to_string();
    assert!(
        err.contains("signature") || err.contains("registry_signature_mismatch"),
        "unexpected error: {err}"
    );
}

#[test]
fn registry_version_requirements_support_exact_and_caret_boundaries() {
    let exact =
        package::parse_version_requirement("1.2.3", Default::default()).expect("exact requirement");
    let caret = package::parse_version_requirement("^1.2.3", Default::default())
        .expect("caret requirement");
    let zero_minor = package::parse_version_requirement("^0.2.3", Default::default())
        .expect("zero-major caret requirement");
    let zero_patch = package::parse_version_requirement("^0.0.3", Default::default())
        .expect("zero-minor caret requirement");
    let version =
        |value| package::parse_package_version(value, Default::default()).expect("package version");

    assert!(package::version_requirement_matches(
        exact,
        version("1.2.3")
    ));
    assert!(!package::version_requirement_matches(
        exact,
        version("1.2.4")
    ));
    assert!(package::version_requirement_matches(
        caret,
        version("1.9.0")
    ));
    assert!(!package::version_requirement_matches(
        caret,
        version("2.0.0")
    ));
    assert!(package::version_requirement_matches(
        zero_minor,
        version("0.2.99")
    ));
    assert!(!package::version_requirement_matches(
        zero_minor,
        version("0.3.0")
    ));
    assert!(package::version_requirement_matches(
        zero_patch,
        version("0.0.3")
    ));
    assert!(!package::version_requirement_matches(
        zero_patch,
        version("0.0.4")
    ));

    let err = package::parse_version_requirement("~1.2.3", Default::default())
        .expect_err("tilde requirement should be rejected");
    assert_eq!(err.code.as_deref(), Some("invalid_version_requirement"));
}

#[test]
fn registry_resolver_selects_highest_shared_version_and_reports_conflicts() {
    let checksum = format!("sha256-{}", "a".repeat(64));
    let manifest = |version: &str| package::RegistryManifest {
        name: "math".to_string(),
        version: version.to_string(),
        source: format!("https://registry.example/math/{version}.tar.zst"),
        checksum: checksum.clone(),
    };
    let dependency = |requirement: &str| package::PackageDependency {
        name: "math".to_string(),
        version: requirement.to_string(),
        source: None,
        checksum: None,
    };
    let manifests = vec![manifest("1.2.3"), manifest("1.8.0"), manifest("2.0.0")];

    let resolved = package::resolve_registry_dependencies(
        &[dependency("^1.2.0"), dependency("^1.7.0")],
        &manifests,
        Default::default(),
    )
    .expect("compatible requirements");
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].version, "1.8.0");

    let err = package::resolve_registry_dependencies(
        &[dependency("1.2.3"), dependency("^2.0.0")],
        &manifests,
        Default::default(),
    )
    .expect_err("incompatible requirements should fail");
    assert_eq!(err.code.as_deref(), Some("dependency_conflict"));
}

#[test]
fn registry_download_plan_is_offline_bounded_and_cache_aware() {
    let manifest = package::RegistryManifest {
        name: "math".to_string(),
        version: "1.2.3".to_string(),
        source: "https://registry.example/math/1.2.3.tar.zst".to_string(),
        checksum: format!("sha256-{}", "b".repeat(64)),
    };
    let cache = unique_temp_path("registry-plan");
    let policy = package::RegistryFetchPolicy::default();
    let reuse = package::plan_registry_download(
        &cache,
        &manifest,
        Some(&manifest.checksum),
        policy,
        Default::default(),
    )
    .expect("reuse plan");
    assert_eq!(reuse.action, package::RegistryCacheAction::ReuseVerified);
    assert_eq!(reuse.policy.max_attempts, 3);
    assert!(reuse.target_dir.ends_with(
        PathBuf::from("packages")
            .join("math")
            .join(format!("math-1.2.3-sha256-{}", "b".repeat(64)))
    ));
    assert!(reuse
        .temporary_dir
        .starts_with(cache.join(".registry-downloads")));

    let refresh =
        package::plan_registry_download(&cache, &manifest, None, policy, Default::default())
            .expect("refresh plan");
    assert_eq!(
        refresh.action,
        package::RegistryCacheAction::DownloadAndReplace
    );
    assert_ne!(refresh.target_dir, refresh.temporary_dir);
    let second_refresh =
        package::plan_registry_download(&cache, &manifest, None, policy, Default::default())
            .expect("second refresh plan");
    assert_ne!(
        refresh.temporary_dir, second_refresh.temporary_dir,
        "concurrent registry downloads must not share a temporary directory"
    );

    let err = package::plan_registry_download(
        &cache,
        &manifest,
        None,
        package::RegistryFetchPolicy {
            max_attempts: 0,
            ..policy
        },
        Default::default(),
    )
    .expect_err("zero retry attempts should fail");
    assert_eq!(err.code.as_deref(), Some("invalid_fetch_policy"));

    for policy in [
        package::RegistryFetchPolicy {
            max_attempts: 9,
            ..policy
        },
        package::RegistryFetchPolicy {
            max_download_bytes: 32_000_001,
            ..policy
        },
    ] {
        let err =
            package::plan_registry_download(&cache, &manifest, None, policy, Default::default())
                .expect_err("excessive registry resource policy should fail");
        assert_eq!(err.code.as_deref(), Some("invalid_fetch_policy"));
    }
}

#[test]
fn package_file_dependency_checksum_mismatch_is_rejected() {
    let dir = unique_temp_path("package-remote-dep-bad-checksum");
    let app_src = dir.join("app").join("src");
    let dep_src = dir.join("registry").join("util").join("src");
    fs::create_dir_all(&app_src).expect("create app src");
    fs::create_dir_all(&dep_src).expect("create dep src");
    fs::write(dep_src.join("util.ku"), "fn Value(): int { return 1 }").expect("write dep util");
    let dep_root = dir.join("registry").join("util");
    fs::write(
        dir.join("app").join("ku.mod"),
        format!(
            r#"
name = "demo_pkg"
dep.util = "1.0.0"
dep.util.source = "file://{}"
dep.util.checksum = "ku-fnv64-00000000deadbeef"
"#,
            dep_root.to_string_lossy().replace('\\', "/")
        ),
    )
    .expect("write ku.mod");
    let main = app_src.join("main.ku");
    fs::write(
        &main,
        r#"
import { Value } from "@util/util"
fn main() { print(Value()) }
"#,
    )
    .expect("write main");

    let err = run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect_err("checksum mismatch should fail")
    .to_string();
    assert!(err.contains("checksum mismatch"), "unexpected error: {err}");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn package_source_checksum_rejects_symlink_entries() {
    let dir = unique_temp_path("package-symlink");
    let root = dir.join("dep");
    fs::create_dir_all(&root).expect("create dep root");
    fs::write(root.join("lib.ku"), "fn value(): int { return 1 }\n").expect("write dep");
    let link = root.join("loop");
    if create_dir_symlink(&root, &link).is_err() {
        let _ = fs::remove_dir_all(&dir);
        return;
    }

    let err = package::package_source_checksum(&root)
        .expect_err("package checksum should reject symlink")
        .to_string();
    assert!(
        err.contains("unsupported symlink"),
        "unexpected error: {err}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
fn create_dir_symlink(target: &PathBuf, link: &PathBuf) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_dir_symlink(target: &PathBuf, link: &PathBuf) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

#[test]
fn package_gc_removes_stale_dependency_versions_only() {
    let dir = unique_temp_path("package-gc");
    let app_src = dir.join("app").join("src");
    fs::create_dir_all(&app_src).expect("create app src");
    fs::write(app_src.join("main.ku"), "fn main() { print(\"ok\") }").expect("write main");
    fs::write(
        dir.join("app").join("ku.mod"),
        r#"
name = "demo_pkg"
dep.util = "1.0.0"
dep.util.source = "file://C:/tmp/util"
"#,
    )
    .expect("write ku.mod");
    let cache = dir.join("app").join(".ku").join("cache").join("packages");
    fs::create_dir_all(cache.join("util").join("1.0.0")).expect("create current cache");
    fs::create_dir_all(cache.join("util").join("0.9.0")).expect("create stale version cache");
    fs::create_dir_all(cache.join("old").join("1.0.0")).expect("create stale package cache");
    let package = package::discover_for_file(&app_src.join("main.ku"))
        .expect("discover")
        .expect("package");

    let removed = package::gc_cache(&package, 64).expect("gc cache");

    assert_eq!(removed, 2);
    assert!(cache.join("util").join("1.0.0").exists());
    assert!(!cache.join("util").join("0.9.0").exists());
    assert!(!cache.join("old").exists());

    let _ = fs::remove_dir_all(dir);
}
