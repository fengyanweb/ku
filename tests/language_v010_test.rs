#[path = "support/bounded_process.rs"]
pub mod bounded_process;

use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::PathBuf,
    process::Command,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bounded_process::{run_bounded, OutputLimits};

use ku::{
    backend, checker::Checker, cli::check_source, cli::run_cli, cli::run_source, ir, lexer::Lexer,
    package, parser::Parser,
};

const NATIVE_RUN_TIMEOUT: Duration = Duration::from_secs(20);
const NATIVE_RUN_OUTPUT_LIMITS: OutputLimits = OutputLimits::new(4 * 1024 * 1024, 6 * 1024 * 1024);

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
    let program = Parser::new(tokens).parse_program().expect("parse");
    Checker::new().check(&program).expect("check");
    ir::lower_program(&program).expect("lower ir").to_string()
}

fn check_err(source: &str) -> String {
    let tokens = Lexer::new(source).tokenize().expect("lex");
    let program = Parser::new(tokens).parse_program().expect("parse");
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
                ir::IrTerminator::Safepoint {
                    continue_block,
                    timeout_block,
                } => vec![*continue_block, *timeout_block],
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
    let (catch_name, error_source) = text
        .lines()
        .find_map(|line| {
            line.trim_start()
                .strip_prefix("bind_error ")?
                .split_once(" from ")
        })
        .expect("catch must bind the propagated error");
    assert!(
        catch_name.starts_with("__ku_local_") && catch_name.ends_with("_err"),
        "unexpected IR:\n{text}"
    );
    assert!(
        error_source.starts_with("__ku_error_"),
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
    let program = Parser::new(tokens).parse_program().expect("parse");
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
    let program = Parser::new(tokens).parse_program().expect("parse");
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
fn native_build_native_emits_local_import_graph_without_runner_source_loader() {
    let dir = unique_temp_path("native-import-graph");
    let src = dir.join("src");
    fs::create_dir_all(&src).expect("create temp src");
    let main = src.join("main.ku");
    fs::write(
        src.join("math.ku"),
        r#"
fn Add(a:int, b:int): int {
    return a + b
}
"#,
    )
    .expect("write imported module");
    fs::write(
        &main,
        r#"
import { Add } from "./math.ku"

fn main(): null! {
    println(Add(1, 2))
    return ok(null)
}
"#,
    )
    .expect("write entry module");

    run_cli(vec![
        "ku".to_string(),
        "build".to_string(),
        "--native".to_string(),
        main.display().to_string(),
    ])
    .expect("ku build --native C artifact should include import graph");

    // `ku build --native <file>` without `-o` is the no-link compatibility path:
    // it must emit this adjacent C artifact even when no C compiler is installed.
    let c_path = main.with_extension("c");
    let c = fs::read_to_string(&c_path).expect("read generated C");
    assert!(
        c.lines().any(|line| {
            line.starts_with("int64_t __ku_import") && line.contains("_Add(int64_t a, int64_t b)")
        }),
        "imported Add missing:\n{c}"
    );
    assert!(
        c.contains("KuResult_null ku_main(void)"),
        "entry main missing:\n{c}"
    );
    assert!(
        !c.contains("run_source") && !c.contains("const SOURCE"),
        "native C artifact must not use runner source loader:\n{c}"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn import_graph_depth_fails_with_a_bounded_structured_error() {
    let dir = unique_temp_path("import-depth-budget");
    fs::create_dir_all(&dir).expect("create import depth directory");
    let module_count = 140usize;
    for index in (0..module_count).rev() {
        let source = if index + 1 == module_count {
            format!("fn F{index}(): int {{ return {index} }}\n")
        } else {
            format!(
                "import {{ F{} }} from \"./m{}\"\nfn F{index}(): int {{ return F{}() }}\n",
                index + 1,
                index + 1,
                index + 1
            )
        };
        fs::write(dir.join(format!("m{index}.ku")), source).expect("write depth module");
    }
    let main = dir.join("main.ku");
    fs::write(
        &main,
        "import { F0 } from \"./m0\"\nfn main() { println(F0()) }\n",
    )
    .expect("write depth entry");

    let started = Instant::now();
    let err = run_cli(vec![
        "ku".to_string(),
        "build".to_string(),
        "--native".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect_err("deep import graph must fail before exhausting the Rust stack");
    assert_eq!(
        err.domain.as_deref(),
        Some("import"),
        "unexpected error: {err}"
    );
    assert_eq!(
        err.code.as_deref(),
        Some("depth_limit"),
        "unexpected error: {err}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "deep import rejection was not prompt: {:?}",
        started.elapsed()
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn import_graph_width_fails_at_the_unique_module_budget() {
    let dir = unique_temp_path("import-module-budget");
    fs::create_dir_all(&dir).expect("create import width directory");
    let imported_modules = 4_096usize;
    let mut main_source = String::new();
    for index in 0..imported_modules {
        fs::write(
            dir.join(format!("m{index}.ku")),
            format!("fn F{index}(): int {{ return {index} }}\n"),
        )
        .expect("write width module");
        main_source.push_str(&format!(
            "import {{ F{index} as _F{index} }} from \"./m{index}\"\n"
        ));
    }
    main_source.push_str("fn main() {}\n");
    let main = dir.join("main.ku");
    fs::write(&main, main_source).expect("write width entry");

    let started = Instant::now();
    let err = run_cli(vec![
        "ku".to_string(),
        "build".to_string(),
        "--native".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect_err("wide import graph must fail at the unique module budget");
    assert_eq!(
        err.domain.as_deref(),
        Some("import"),
        "unexpected error: {err}"
    );
    assert_eq!(
        err.code.as_deref(),
        Some("module_limit"),
        "unexpected error: {err}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "wide import rejection was not bounded: {:?}",
        started.elapsed()
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn repeated_namespace_imports_materialize_each_module_once() {
    let dir = unique_temp_path("import-clone-budget");
    fs::create_dir_all(&dir).expect("create import clone directory");
    fs::write(dir.join("layer0.ku"), "fn Value0(): int { return 1 }\n").expect("write base layer");
    let layers = 18usize;
    for index in 1..=layers {
        fs::write(
            dir.join(format!("layer{index}.ku")),
            format!(
                "import left{index} from \"./layer{}\"\nimport right{index} from \"./layer{}\"\nfn Value{index}(): int {{ return left{index}.Value{}() + right{index}.Value{}() }}\n",
                index - 1,
                index - 1,
                index - 1,
                index - 1
            ),
        )
        .expect("write repeated namespace layer");
    }
    let main = dir.join("main.ku");
    fs::write(
        &main,
        format!(
            "import root from \"./layer{layers}\"\nfn main() {{ println(root.Value{layers}()) }}\n"
        ),
    )
    .expect("write repeated namespace entry");

    let started = Instant::now();
    run_cli(vec![
        "ku".to_string(),
        "build".to_string(),
        "--native".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect("diamond namespace edges should reuse canonical module declarations");
    let c = fs::read_to_string(main.with_extension("c")).expect("read canonical import C");
    let definitions = c
        .lines()
        .filter(|line| line.starts_with("int64_t __ku_import") && line.ends_with(" {"))
        .count();
    assert!(
        definitions == layers + 1,
        "expected one function definition per source module, got {definitions}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "canonical import expansion was not prompt: {:?}",
        started.elapsed()
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn diamond_imports_share_one_nominal_type_and_emit_deterministic_native_c() {
    let dir = unique_temp_path("diamond-type-identity");
    let shared = dir.join("shared");
    fs::create_dir_all(&shared).expect("create diamond import directory");
    fs::write(
        shared.join("token.ku"),
        r#"
struct Token { value: int }
fn Make(value: int): Token { return Token { value: value } }
fn Canonical(token: Token): int { return token.value }
"#,
    )
    .expect("write shared token module");
    fs::write(
        dir.join("lexer.ku"),
        r#"
import { Token, Make } from "./shared/token"
fn Scan(): [Token] { return [Make(7)] }
"#,
    )
    .expect("write lexer module");
    let main = dir.join("main.ku");
    fs::write(
        &main,
        r#"
import { Scan } from "./lexer"
import { Token as DirectToken, Canonical as CanonicalA } from "./shared/token"
import { Make as MakeAgain, Canonical as CanonicalB } from "./shared/../shared/token"

fn main(): null! {
    tokens = Scan()
    first: DirectToken = tokens[0].clone()
    if (CanonicalA(first) != 7) { panic("diamond type identity split") }
    second = MakeAgain(8)
    if (CanonicalB(second) != 8) { panic("same-module aliases split") }
    return ok(null)
}
"#,
    )
    .expect("write diamond entry");

    run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect("diamond nominal type must check");
    run_cli(vec![
        "ku".to_string(),
        "run".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect("diamond nominal type must run");
    let build = || {
        run_cli(vec![
            "ku".to_string(),
            "build".to_string(),
            "--native".to_string(),
            main.to_string_lossy().to_string(),
        ])
        .expect("diamond nominal type must lower to native C");
        fs::read(main.with_extension("c")).expect("read diamond native C")
    };
    let first_c = build();
    let second_c = build();
    assert_eq!(
        first_c, second_c,
        "native import symbols must be deterministic"
    );
    let c = String::from_utf8(first_c).expect("native C is UTF-8");
    let token_definitions = c
        .lines()
        .filter(|line| line.starts_with("struct KuStruct___ku_import") && line.contains("_Token {"))
        .count();
    assert_eq!(
        token_definitions, 1,
        "the canonical Token declaration must be materialized once:\n{c}"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn namespace_function_values_respect_local_shadowing() {
    let dir = unique_temp_path("namespace-function-value-shadow");
    fs::create_dir_all(&dir).expect("create namespace shadow directory");
    fs::write(
        dir.join("math.ku"),
        "fn Add(a: int, b: int): int { return a + b }\n",
    )
    .expect("write namespace module");
    let main = dir.join("main.ku");
    fs::write(
        &main,
        r#"
import math from "./math"
struct Holder { Add: int }
fn Shadow(math: Holder): int { return math.Add }
fn main() {
    op: fn(int, int): int = math.Add
    if (op(2, 3) != 5) { panic("namespace function value was not rewritten") }
    math = Holder { Add: 9 }
    if (math.Add != 9) { panic("local namespace shadow was rewritten") }
    if (Shadow(math) != 9) { panic("parameter namespace shadow was rewritten") }
}
"#,
    )
    .expect("write namespace shadow entry");

    run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect("namespace function values and shadows must check");
    run_cli(vec![
        "ku".to_string(),
        "run".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect("namespace function values and shadows must run");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn imports_are_private_bindings_not_implicit_reexports() {
    let dir = unique_temp_path("import-no-reexport");
    fs::create_dir_all(&dir).expect("create import privacy directory");
    fs::write(dir.join("origin.ku"), "fn Value(): int { return 1 }\n")
        .expect("write origin module");
    fs::write(
        dir.join("named.ku"),
        "import { Value } from \"./origin\"\nfn Named(): int { return Value() }\n",
    )
    .expect("write named relay");
    fs::write(
        dir.join("glob.ku"),
        "import \"./origin\"\nfn Glob(): int { return Value() }\n",
    )
    .expect("write glob relay");
    fs::write(
        dir.join("namespace.ku"),
        "import origin from \"./origin\"\nfn Namespace(): int { return origin.Value() }\n",
    )
    .expect("write namespace relay");

    for relay in ["named", "glob", "namespace"] {
        let main = dir.join(format!("bad-{relay}.ku"));
        fs::write(
            &main,
            format!("import {{ Value }} from \"./{relay}\"\nfn main() {{ println(Value()) }}\n"),
        )
        .expect("write reexport probe");
        let err = run_cli(vec![
            "ku".to_string(),
            "check".to_string(),
            main.to_string_lossy().to_string(),
        ])
        .expect_err("an imported binding must not be reexported")
        .to_string();
        assert!(
            err.contains("'Value' is not exported"),
            "unexpected {relay} reexport diagnostic: {err}"
        );
    }
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn same_named_types_from_different_modules_remain_nominally_distinct() {
    let dir = unique_temp_path("distinct-import-types");
    fs::create_dir_all(&dir).expect("create distinct type directory");
    fs::write(
        dir.join("left.ku"),
        "struct Token { value: int }\nfn MakeLeft(): Token { return Token { value: 1 } }\n",
    )
    .expect("write left type module");
    fs::write(
        dir.join("right.ku"),
        "struct Token { value: int }\nfn AcceptRight(token: Token): int { return token.value }\n",
    )
    .expect("write right type module");
    let main = dir.join("main.ku");
    fs::write(
        &main,
        "import { MakeLeft } from \"./left\"\nimport { AcceptRight } from \"./right\"\nfn main() { println(AcceptRight(MakeLeft())) }\n",
    )
    .expect("write distinct type entry");
    let err = run_cli(vec![
        "ku".to_string(),
        "build".to_string(),
        "--native".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect_err("different modules' Token declarations must stay distinct")
    .to_string();
    assert!(
        err.contains("type error"),
        "unexpected nominal error: {err}"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn repeated_import_edges_hit_a_structured_hard_limit() {
    let dir = unique_temp_path("import-edge-budget");
    fs::create_dir_all(&dir).expect("create import edge budget directory");
    fs::write(dir.join("empty.ku"), "fn hidden(): int { return 1 }\n")
        .expect("write private dependency");
    let mut source = String::new();
    for _ in 0..16_385 {
        source.push_str("import \"./empty\"\n");
    }
    source.push_str("fn main() {}\n");
    let main = dir.join("main.ku");
    fs::write(&main, source).expect("write import edge budget entry");
    let started = Instant::now();
    let err = run_cli(vec![
        "ku".to_string(),
        "build".to_string(),
        "--native".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect_err("excessive repeated import edges must fail");
    assert_eq!(
        err.domain.as_deref(),
        Some("import"),
        "unexpected import edge error: {err}"
    );
    assert_eq!(
        err.code.as_deref(),
        Some("edge_limit"),
        "unexpected import edge error: {err}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "import edge rejection was not prompt: {:?}",
        started.elapsed()
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn diamond_std_imports_are_deduplicated() {
    let dir = unique_temp_path("diamond-std-import");
    fs::create_dir_all(&dir).expect("create diamond std directory");
    for (name, function) in [("left", "Left"), ("right", "Right")] {
        fs::write(
            dir.join(format!("{name}.ku")),
            format!(
                "import time from \"std.time\"\nfn {function}(): int {{ return time.millis() }}\n"
            ),
        )
        .expect("write std dependency branch");
    }
    let main = dir.join("main.ku");
    fs::write(
        &main,
        "import { Left } from \"./left\"\nimport { Right } from \"./right\"\nfn main() { println(Left() <= Right()) }\n",
    )
    .expect("write diamond std entry");
    run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect("the same std module imported on two branches must be deduplicated");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn named_imports_keep_internal_dependency_closure_without_export_leaks() {
    let dir = unique_temp_path("named-import-closure");
    fs::create_dir_all(&dir).expect("create named import directory");
    fs::write(
        dir.join("left.ku"),
        r#"
struct Box {
    value: int
}

fn Shared(): int { return 100 }
fn MakeBox(): Box { return Box { value: 40 } }
fn Pick(): int { return MakeBox().value + Shared() }
"#,
    )
    .expect("write left module");
    fs::write(
        dir.join("right.ku"),
        r#"
fn Shared(): int { return 200 }
fn Use(): int { return Shared() }
"#,
    )
    .expect("write right module");
    let main = dir.join("main.ku");
    fs::write(
        &main,
        r#"
import { Pick } from "./left"
import { Use } from "./right"

fn main() {
    if (Pick() != 140) { panic("left dependency closure was not preserved") }
    if (Use() != 200) { panic("right dependency closure was not preserved") }
}
"#,
    )
    .expect("write named import entry");

    run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect("selected functions must retain unselected helper and type dependencies");
    run_cli(vec![
        "ku".to_string(),
        "run".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect("independent modules with same-named unselected exports must run");

    let leak = dir.join("leak.ku");
    fs::write(
        &leak,
        "import { Pick } from \"./left\"\nfn main() { println(Pick()); println(Shared()) }\n",
    )
    .expect("write export leak probe");
    let err = run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        leak.to_string_lossy().to_string(),
    ])
    .expect_err("an unselected public helper must not remain visible")
    .to_string();
    assert!(
        err.contains("undefined function 'Shared'"),
        "unexpected leak error: {err}"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn named_import_rewrites_top_level_function_values_but_respects_local_shadowing() {
    let dir = unique_temp_path("named-import-function-values");
    fs::create_dir_all(&dir).expect("create function value import directory");
    fs::write(
        dir.join("ops.ku"),
        r#"
fn Add(a: int, b: int): int { return a + b }
fn Apply(op: fn(int, int): int, a: int, b: int): int { return op(a, b) }

fn Run(): int {
    first = Apply(Add, 1, 2)
    op: fn(int, int): int = Add
    closure = () => { return Add(3, 4) }
    return first + op(2, 3) + closure()
}

fn Shadow(Add: int): int { return Add + 1 }

fn Branch(flag: bool): int {
    if (flag) {
        Add = 10
        if (Add != 10) { panic("then-branch local was rewritten") }
    } else {
        Add = 20
        if (Add != 20) { panic("else-branch local was rewritten") }
    }
    return Add(4, 5)
}
"#,
    )
    .expect("write function value module");
    let main = dir.join("main.ku");
    fs::write(
        &main,
        r#"
import { Run, Shadow, Branch } from "./ops"

fn main() {
    if (Run() != 15) { panic("top-level function value references were not rewritten") }
    if (Shadow(5) != 6) { panic("local shadow was rewritten as a top-level helper") }
    if (Branch(true) != 9) { panic("then-branch scope escaped or hid the helper") }
    if (Branch(false) != 9) { panic("else-branch scope escaped or hid the helper") }
}
"#,
    )
    .expect("write function value entry");

    run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect("function value and local shadow import must check");
    run_cli(vec![
        "ku".to_string(),
        "run".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect("function value and local shadow import must run");
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
fn native_ir_rejects_closure_capture_of_for_binding_explicitly() {
    let tokens = Lexer::new(
        r#"
fn main() {
    for value in [1, 2] {
        get: fn(): int = () => value
        println(get())
    }
}
"#,
    )
    .tokenize()
    .expect("lex");
    let program = Parser::new(tokens).parse_program().expect("parse");
    Checker::new().check(&program).expect("check");
    let err = ir::lower_program(&program)
        .expect_err("native for-binding capture must be rejected until per-iteration cells exist")
        .to_string();
    assert!(
        err.contains("closure capture of a for loop variable"),
        "unexpected error: {err}"
    );
}

#[test]
fn compiler_reserved_namespace_prevents_for_state_name_collision() {
    let tokens = Lexer::new(
        r#"
fn main() {
    __ku_for_1_array = 7
    for value in [1, 2] { println(value) }
}
"#,
    )
    .tokenize()
    .expect("lex");
    let program = Parser::new(tokens).parse_program().expect("parse");
    let err = Checker::new()
        .check(&program)
        .expect_err("compiler-reserved for state names must reject user bindings")
        .to_string();
    assert!(
        err.contains("identifiers starting with '__ku_' are used by the compiler"),
        "unexpected error: {err}"
    );
}

#[test]
fn native_int_for_uses_non_wrapping_64_bit_counter() {
    let tokens = Lexer::new(
        r#"
fn main() {
    for value in 3 { println(value) }
}
"#,
    )
    .tokenize()
    .expect("lex");
    let program = Parser::new(tokens).parse_program().expect("parse");
    Checker::new().check(&program).expect("check");
    let ir = ir::lower_program(&program).expect("lower ir");
    let c = backend::c::generate_c_source(&ir).expect("generate native for C");
    assert!(
        c.lines().any(|line| {
            let line = line.trim();
            line.starts_with("uint64_t __ku_for_") && line.ends_with("_index = 0;")
        }),
        "int for must use a 64-bit counter on 32-bit targets"
    );
}

#[test]
fn native_c_backend_lowers_kustring_static_clone_concat_and_drop() {
    let tokens = Lexer::new(
        r#"
fn main() {
    name = "Ku"
    again = name.clone()
    text = "Hello " + again
    println(text)
}
"#,
    )
    .tokenize()
    .expect("lex");
    let program = Parser::new(tokens).parse_program().expect("parse");
    Checker::new().check(&program).expect("check");
    let ir = ir::lower_program(&program).expect("lower");
    let c = backend::c::generate_c_source(&ir).expect("generate C with KuString");
    assert!(
        c.contains("typedef struct KuString"),
        "missing KuString ABI:\n{c}"
    );
    assert!(
        c.contains("ku_string_static"),
        "missing static string literal:\n{c}"
    );
    assert!(c.contains("ku_string_clone"), "missing string clone:\n{c}");
    assert!(
        c.contains("ku_string_concat"),
        "missing string concat:\n{c}"
    );
    assert!(c.contains("ku_string_drop"), "missing string drop:\n{c}");
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
    let program = Parser::new(tokens).parse_program().expect("parse");
    Checker::new().check(&program).expect("check");
    let ir = ir::lower_program(&program).expect("lower");
    let c = backend::c::generate_c_source(&ir).expect("generate c");

    assert!(c.contains("KuResult_int ku_main("));
    assert!(c.contains("int main(void)"));
    assert!(c.contains("typedef struct KuResult_int KuResult_int;"));
    assert!(c.contains("struct KuResult_int { bool ok; int64_t value; KuError error; };"));
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
    let program = Parser::new(tokens).parse_program().expect("parse");
    Checker::new().check(&program).expect("check");
    let ir = ir::lower_program(&program).expect("lower");
    let c = backend::c::generate_c_source(&ir).expect("generate array Result C");
    assert!(c.contains("typedef struct KuResult_array_int KuResult_array_int;"));
    assert!(c.contains("struct KuResult_array_int { bool ok; KuArray_int value; KuError error; };"));
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
    let program = Parser::new(tokens).parse_program().expect("parse");
    Checker::new().check(&program).expect("check");
    let ir = ir::lower_program(&program).expect("lower");
    let text = ir.to_string();
    assert!(text.contains("finally_return"), "unexpected IR:\n{text}");
    assert!(text.contains("__ku_return_"), "unexpected IR:\n{text}");
    assert!(text.contains("__ku_error_"), "unexpected IR:\n{text}");

    let c = backend::c::generate_c_source(&ir).expect("generate try/finally C");
    let catch_name = ir
        .functions
        .iter()
        .flat_map(|function| &function.blocks)
        .flat_map(|block| &block.instructions)
        .find_map(|instruction| match instruction {
            ir::IrInst::BindError { name, .. } => Some(name),
            _ => None,
        })
        .expect("try/catch must have an error binding");
    assert!(c.contains("typedef struct KuError"));
    assert!(c.contains(&format!("KuError {catch_name} = ")));
    assert!(c.contains(&format!("ku_error_drop(&{catch_name})")));
    assert!(c.contains("ku_error_move(&"));
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
    let program = Parser::new(tokens).parse_program().expect("parse");
    Checker::new().check(&program).expect("check");
    let ir = ir::lower_program(&program).expect("lower");
    let c = backend::c::generate_c_source(&ir).expect("generate c");

    assert!(
        c.contains("typedef struct { size_t len; int64_t* data; size_t capacity; } KuArray_int")
    );
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
fn use_of_moved_value_reports_e0901() {
    let source = r#"
fn take(values:[int]): null {
    return null
}

fn main() {
    values = [1, 2]
    take(values)
    print(values[0])
}
"#;
    let tokens = Lexer::new(source).tokenize().expect("lex");
    let program = Parser::new(tokens).parse_program().expect("parse");
    let err = Checker::new().check(&program).expect_err("should fail");
    let diagnostic = err.diagnostic("main.ku", source);
    assert!(
        diagnostic.contains("E0901"),
        "use-of-moved must report E0901:\n{diagnostic}"
    );
    assert!(diagnostic.contains("use of moved value 'values'"));
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
    let program = Parser::new(tokens).parse_program().expect("parse");
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
    let program = Parser::new(tokens).parse_program().expect("parse");
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
        loop_move.contains("moved") && loop_move.contains("values"),
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
    let program = Parser::new(tokens).parse_program().expect("parse");
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
    let program = Parser::new(tokens).parse_program().expect("parse");
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
fn ordinary_code_cannot_schedule_runtime_tasks_manually() {
    for (api, source) in [
        (
            "task.spawn",
            r#"import "std.task" fn main() { task.spawn(() => { return null }) }"#,
        ),
        (
            "Task.new",
            r#"fn main() { Task.new(() => { return null }) }"#,
        ),
        (
            "runtime.schedule",
            r#"fn main() { runtime.schedule(() => { return null }) }"#,
        ),
    ] {
        let error = check_source("inline.ku", source)
            .expect_err("manual task scheduling must stay outside the user API")
            .to_string();
        assert!(
            error.contains("unknown stdlib function") || error.contains("undefined variable"),
            "{api} unexpectedly reached a callable user API: {error}"
        );
    }
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
    let program = Parser::new(tokens).parse_program().expect("parse");
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
    let program = Parser::new(tokens).parse_program().expect("parse");
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
    let program = Parser::new(tokens).parse_program().expect("parse");
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
    let program = Parser::new(tokens).parse_program().expect("parse");
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
        err.contains("http url must be an absolute http:// or https:// URL"),
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
    let util_cache = dir
        .join("app")
        .join(".ku")
        .join("cache")
        .join("packages")
        .join("util");
    let cached_roots = fs::read_dir(&util_cache)
        .expect("read util cache")
        .map(|entry| entry.expect("read cache entry").path())
        .collect::<Vec<_>>();
    assert_eq!(cached_roots.len(), 1, "unexpected cache roots");
    assert!(
        cached_roots[0].join("src").join("util.ku").exists(),
        "dependency should be cached"
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn package_multifile_dependency_imports_work_for_author_consumer_and_native_build() {
    let dir = unique_temp_path("package-multifile-import");
    let util = dir.join("util");
    let util_src = util.join("src");
    let app = dir.join("app");
    let app_src = app.join("src");
    fs::create_dir_all(util_src.join("internal")).expect("create util source");
    fs::create_dir_all(&app_src).expect("create app source");
    fs::write(
        util.join("ku.mod"),
        "name = \"util\"\nversion = \"1.0.0\"\n",
    )
    .expect("write util manifest");
    fs::write(
        util_src.join("internal").join("base.ku"),
        "fn Base(): int { return 40 }\n",
    )
    .expect("write util base module");
    fs::write(
        util_src.join("layer.ku"),
        "import { Base } from \"internal/base\"\nfn AddTwo(): int { return Base() + 2 }\n",
    )
    .expect("write util layer module");
    fs::write(
        util_src.join("entry.ku"),
        "import { AddTwo } from \"./layer\"\nfn Value(): int { return AddTwo() }\n",
    )
    .expect("write util public entry module");
    let util_main = util_src.join("main.ku");
    fs::write(
        &util_main,
        "import { Value } from \"entry\"\nfn main() { if (Value() != 42) { panic(\"bad author import\") } }\n",
    )
    .expect("write util author entry");

    run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        util_main.to_string_lossy().to_string(),
    ])
    .expect("author package check");
    run_cli(vec![
        "ku".to_string(),
        "run".to_string(),
        util_main.to_string_lossy().to_string(),
    ])
    .expect("author package run");
    run_cli(vec![
        "ku".to_string(),
        "package".to_string(),
        "pack".to_string(),
        util.to_string_lossy().to_string(),
    ])
    .expect("author package pack");

    let checksum = package::package_source_checksum(&util).expect("util checksum");
    fs::write(
        app.join("ku.mod"),
        format!(
            "name = \"app\"\nversion = \"0.1.0\"\ndep.util = \"1.0.0\"\ndep.util.source = \"file://{}\"\ndep.util.checksum = \"{}\"\n",
            util.to_string_lossy().replace('\\', "/"),
            checksum
        ),
    )
    .expect("write app manifest");
    let app_main = app_src.join("main.ku");
    fs::write(
        &app_main,
        "import { Value } from \"@util/entry\"\nfn main() { if (Value() != 42) { panic(\"bad consumer import\") } }\n",
    )
    .expect("write app entry");

    run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        app_main.to_string_lossy().to_string(),
    ])
    .expect("consumer package check");
    let app_lock = fs::read_to_string(app.join("ku.lock")).expect("read app lock");
    for dependency_path in ["@util/entry.ku", "@util/layer.ku", "@util/internal/base.ku"] {
        assert!(
            app_lock.contains(&format!("path = \"{dependency_path}\"")),
            "portable dependency path missing from lock:\n{app_lock}"
        );
    }
    assert!(
        !app_lock.contains("path = \".ku/cache/"),
        "lock must not expose the local cache layout:\n{app_lock}"
    );
    run_cli(vec![
        "ku".to_string(),
        "package".to_string(),
        "resolve".to_string(),
        app.to_string_lossy().to_string(),
        "--offline".to_string(),
    ])
    .expect("consumer offline resolution from lock and cache");
    run_cli(vec![
        "ku".to_string(),
        "run".to_string(),
        app_main.to_string_lossy().to_string(),
    ])
    .expect("consumer package run");
    run_cli(vec![
        "ku".to_string(),
        "build".to_string(),
        "--native".to_string(),
        app_main.to_string_lossy().to_string(),
    ])
    .expect("consumer native C build");
    let c = fs::read_to_string(app_main.with_extension("c")).expect("read native C");
    assert!(c.contains("Base("), "internal base function missing:\n{c}");
    assert!(
        c.contains("AddTwo("),
        "relative layer function missing:\n{c}"
    );
    assert!(
        c.contains("Value("),
        "dependency entry function missing:\n{c}"
    );
    assert!(!c.contains("run_source") && !c.contains("const SOURCE"));

    let binary = app.join(if cfg!(windows) {
        "multifile.exe"
    } else {
        "multifile"
    });
    match run_cli(vec![
        "ku".to_string(),
        "build".to_string(),
        "--native".to_string(),
        app_main.to_string_lossy().to_string(),
        "-o".to_string(),
        binary.to_string_lossy().to_string(),
    ]) {
        Ok(()) => {
            let mut command = Command::new(&binary);
            let output = run_bounded(&mut command, NATIVE_RUN_TIMEOUT, NATIVE_RUN_OUTPUT_LIMITS)
                .unwrap_or_else(|error| {
                    panic!("native dependency binary was not bounded: {error}")
                });
            assert!(
                output.status.success(),
                "native dependency binary failed: {:?}\nstdout:\n{}\nstderr:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Err(err) if err.to_string().contains("C compiler not found") => {
            eprintln!("skip linked native package test: no C compiler available");
        }
        Err(err) => panic!("consumer linked native build failed: {err}"),
    }

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
fn package_internal_relative_import_cannot_escape_its_owner_root() {
    let dir = unique_temp_path("package-local-import-escape");
    let src = dir.join("src");
    fs::create_dir_all(&src).expect("create package src");
    fs::write(
        dir.join("ku.mod"),
        "name = \"demo_pkg\"\nversion = \"0.1.0\"\n",
    )
    .expect("write package manifest");
    fs::write(dir.join("secret.ku"), "fn Secret(): int { return 1 }\n")
        .expect("write outside-root module");
    let main = src.join("main.ku");
    fs::write(
        &main,
        "import { Secret } from \"../secret\"\nfn main() { print(Secret()) }\n",
    )
    .expect("write package entry");

    let err = run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect_err("relative import outside the owner root must fail")
    .to_string();
    assert!(
        err.contains("outside package 'demo_pkg' import root"),
        "unexpected error: {err}"
    );
    let _ = fs::remove_dir_all(dir);
}

#[cfg(windows)]
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
    let first_lock = fs::read_to_string(dir.join("app").join("ku.lock")).expect("read first lock");
    let first_cache_key = first_lock
        .lines()
        .rev()
        .find_map(|line| {
            line.trim()
                .strip_prefix("cache_key = \"")?
                .strip_suffix('"')
        })
        .expect("file cache key in lock")
        .to_string();
    fs::write(&dep_file, "fn Value(): int { return 2 }").expect("update dep util");
    run_cli(vec![
        "ku".to_string(),
        "package".to_string(),
        "resolve".to_string(),
        dir.join("app").to_string_lossy().to_string(),
        "--offline".to_string(),
    ])
    .expect("offline resolve must reuse the locked cache without reading changed file source");
    let cached_path = dir
        .join("app")
        .join(".ku")
        .join("cache")
        .join("packages")
        .join("util")
        .join(&first_cache_key);
    let cached_before_refresh =
        fs::read_to_string(cached_path.join("src").join("util.ku")).expect("read locked cache");
    assert!(cached_before_refresh.contains("return 1"));
    fs::remove_dir_all(&cached_path).expect("remove locked cache to exercise locked refill");
    let locked_error = run_cli(vec![
        "ku".to_string(),
        "package".to_string(),
        "resolve".to_string(),
        dir.join("app").to_string_lossy().to_string(),
        "--locked".to_string(),
    ])
    .expect_err("locked resolve must not refresh a changed file dependency");
    assert_eq!(
        locked_error.code.as_deref(),
        Some("locked_source_changed"),
        "unexpected error: {locked_error:?}"
    );
    run_cli(vec![
        "ku".to_string(),
        "check".to_string(),
        main.to_string_lossy().to_string(),
    ])
    .expect("second package check");
    let second_lock =
        fs::read_to_string(dir.join("app").join("ku.lock")).expect("read second lock");
    let second_cache_key = second_lock
        .lines()
        .rev()
        .find_map(|line| {
            line.trim()
                .strip_prefix("cache_key = \"")?
                .strip_suffix('"')
        })
        .expect("refreshed file cache key in lock");
    assert_ne!(first_cache_key, second_cache_key);
    let cached = fs::read_to_string(
        dir.join("app")
            .join(".ku")
            .join("cache")
            .join("packages")
            .join("util")
            .join(second_cache_key)
            .join("src")
            .join("util.ku"),
    )
    .expect("read cached util");
    assert!(cached.contains("return 2"), "unexpected cache:\n{cached}");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn package_file_dependency_rejects_source_containing_consumer_cache() {
    let dir = unique_temp_path("package-dep-self-source");
    let app = dir.join("app");
    fs::create_dir_all(app.join("src")).expect("create app source");
    fs::write(app.join("src").join("main.ku"), "fn main() {}").expect("write app source");
    fs::write(
        app.join("ku.mod"),
        format!(
            "name = \"demo_pkg\"\nversion = \"0.1.0\"\ndep.self_dep = \"1.0.0\"\ndep.self_dep.source = \"file://{}\"\n",
            app.to_string_lossy().replace('\\', "/")
        ),
    )
    .expect("write manifest");

    let err = run_cli(vec![
        "ku".to_string(),
        "package".to_string(),
        "resolve".to_string(),
        app.to_string_lossy().to_string(),
    ])
    .expect_err("a file source containing its destination cache must fail quickly");
    assert_eq!(
        err.code.as_deref(),
        Some("unsafe_file_dependency_source"),
        "unexpected error: {err:?}"
    );
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
    let src = root.join("src");
    fs::create_dir_all(&src).expect("create dep root");
    fs::write(src.join("lib.ku"), "fn value(): int { return 1 }\n").expect("write dep");
    let link = src.join("loop");
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
    let util = dir.join("util");
    fs::create_dir_all(&app_src).expect("create app src");
    fs::create_dir_all(util.join("src")).expect("create util src");
    fs::write(app_src.join("main.ku"), "fn main() { print(\"ok\") }").expect("write main");
    fs::write(
        util.join("ku.mod"),
        "name = \"util\"\nversion = \"1.0.0\"\n",
    )
    .expect("write util manifest");
    fs::write(
        util.join("src").join("util.ku"),
        "fn Value(): int { return 1 }",
    )
    .expect("write util source");
    fs::write(
        dir.join("app").join("ku.mod"),
        format!(
            "name = \"demo_pkg\"\ndep.util = \"1.0.0\"\ndep.util.source = \"file://{}\"\n",
            util.to_string_lossy().replace('\\', "/")
        ),
    )
    .expect("write ku.mod");
    let cache = dir.join("app").join(".ku").join("cache").join("packages");
    fs::create_dir_all(cache.join("util").join("0.9.0")).expect("create stale version cache");
    fs::create_dir_all(cache.join("old").join("1.0.0")).expect("create stale package cache");
    let mut package = package::discover_for_file(&app_src.join("main.ku"))
        .expect("discover")
        .expect("package");
    package::resolve_remote_dependencies(&mut package).expect("resolve file dependency");
    package::write_lock(&package).expect("write file dependency lock");
    let current_cache = package.resolved_file_dependencies[0].package_root.clone();

    let removed = package::gc_cache(&package, 64).expect("gc cache");

    assert_eq!(removed, 2);
    assert!(current_cache.exists());
    assert!(!cache.join("util").join("0.9.0").exists());
    assert!(!cache.join("old").exists());

    run_cli(vec![
        "ku".to_string(),
        "package".to_string(),
        "gc".to_string(),
        dir.join("app").to_string_lossy().to_string(),
    ])
    .expect("package gc must accept the same package directory form as other package commands");

    let _ = fs::remove_dir_all(dir);
}
