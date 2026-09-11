//! Error owner declarations must survive dead finally-copy removal.
//! No timeout or cleanup-catch policy is changed/exercised by this regression.
#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use ku::{
    backend::c,
    checker::Checker,
    ir::{self, IrExprKind, IrInst, IrLValue, IrType},
    lexer::Lexer,
    parser::Parser,
};
use native_allocation_harness::ALLOCATION_HOOK;
use native_harness::{compile_harness, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};
use std::{collections::HashMap, fs, process::Command};

const SOURCE: &str = r#"
fn IntPath(): int! {
    try {
        try { return ok(7) }
        finally { fail { domain: "dom" + "ain", code: "co" + "de", message: "int-" + "failed" } }
    } catch (err) {
        println(err.domain) println(err.code) println(err.message)
        return ok(99)
    }
    return ok(0)
}
fn StrPath(): str! {
    try {
        try { return ok("discarded-" + "return") }
        finally { fail { domain: "dom" + "ain", code: "co" + "de", message: "str-" + "failed" } }
    } catch (err) {
        println(err.domain) println(err.code) println(err.message)
        return ok("handled-" + "str")
    }
    return ok("BAD")
}
fn ArrayPath(): [str]! {
    try {
        try { return ok(["discarded-" + "array"]) }
        finally { fail { domain: "dom" + "ain", code: "co" + "de", message: "array-" + "failed" } }
    } catch (err) {
        println(err.domain) println(err.code) println(err.message)
        return ok(["handled-" + "array"])
    }
    return ok([])
}
fn main() {}
"#;

fn lower() -> ir::IrProgram {
    let ast = Parser::new(Lexer::new(SOURCE).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new()
        .check(&ast)
        .expect("nested return/finally-fail source is legal");
    ir::lower_program(&ast).unwrap()
}

fn assert_error_declarations(program: &ir::IrProgram) {
    let error_type = IrType::Named("__ku_error_type".to_string());
    let mut stores = 0;
    for function in &program.functions {
        let mut declarations = HashMap::new();
        for block in &function.blocks {
            for (index, instruction) in block.instructions.iter().enumerate() {
                if let IrInst::Let { name, ty, value } = instruction {
                    if !name.starts_with("__ku_error_") {
                        continue;
                    }
                    assert_eq!(ty, &error_type);
                    assert_eq!(value.ty, error_type);
                    assert!(
                        matches!(&value.kind, IrExprKind::Literal(text) if text == "<native-zero>")
                    );
                    assert!(
                        declarations
                            .insert(name.as_str(), (block.id, index))
                            .is_none(),
                        "one bare KuError owner per handler: {} {name}",
                        function.name
                    );
                }
            }
        }
        for block in &function.blocks {
            for (index, instruction) in block.instructions.iter().enumerate() {
                match instruction {
                    IrInst::BeginTry { after_block, .. } => {
                        let name = format!("__ku_error_{}", after_block.0);
                        let &(declared_block, declared_index) =
                            declarations.get(name.as_str()).unwrap_or_else(|| {
                                panic!("{} handler has no error owner: {name}", function.name)
                            });
                        assert_eq!(
                            declared_block, block.id,
                            "declaration belongs at handler entry"
                        );
                        assert!(declared_index < index, "error owner precedes BeginTry/body");
                    }
                    IrInst::Store {
                        target: IrLValue::Local(name),
                        value,
                    } if name.starts_with("__ku_error_") => {
                        stores += 1;
                        assert!(
                            declarations.contains_key(name.as_str()),
                            "{} surviving error store has no declaration: {name}",
                            function.name
                        );
                        assert_eq!(
                            value.ty, error_type,
                            "Result payloads share only bare Error"
                        );
                    }
                    IrInst::BindError { result, .. } => {
                        let IrExprKind::Local(name) = &result.kind else {
                            panic!("catch binds handler owner")
                        };
                        assert!(declarations.contains_key(name.as_str()));
                        assert_eq!(result.ty, error_type);
                    }
                    _ => {}
                }
            }
        }
    }
    assert!(
        stores >= 3,
        "all three Result payload paths perform real error stores"
    );
}

#[test]
fn native_error_slots_are_zeroed_at_try_entry_and_survive_optimization() {
    let lowered = lower();
    assert_error_declarations(&lowered);
    let optimized = ir::optimize_program(&lowered);
    assert_error_declarations(&optimized);
    for name in ["IntPath", "StrPath", "ArrayPath"] {
        let before = lowered
            .functions
            .iter()
            .find(|function| function.name == name)
            .unwrap();
        let after = optimized
            .functions
            .iter()
            .find(|function| function.name == name)
            .unwrap();
        assert!(
            after.blocks.len() < before.blocks.len(),
            "dead finally copies really removed: {name}"
        );
    }
}

#[test]
fn native_finally_failure_after_return_compiles_and_releases_error_and_payload_owners() {
    let lowered = lower();
    let optimized = ir::optimize_program(&lowered);
    let generated = c::generate_c_source(&optimized).unwrap();
    let anchor = "typedef struct KuString {";
    assert_eq!(generated.matches(anchor).count(), 1);
    let generated = generated.replacen(anchor, &format!("{ALLOCATION_HOOK}\n{anchor}"), 1);
    assert_eq!(generated.matches("int main(void) {").count(), 1);
    let mut generated = generated.replacen(
        "int main(void) {",
        "static int fixture_unused_source_main(void) {",
        1,
    );
    generated.push_str(C_MAIN);
    let directory = TempDir::new("native-error-slot");
    let path = directory.path().join("program.c");
    fs::write(&path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires a real C compiler"
        );
        return;
    };
    fs::remove_file(path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("finite Error owner regression obeys the real watchdog");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "domain\ncode\nint-failed\ndomain\ncode\nstr-failed\ndomain\ncode\narray-failed\n"
            .repeat(16),
    );
    assert!(output.stderr.is_empty());
}

const C_MAIN: &str = r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr,"error slot check line %d: %s\n",__LINE__,#c); abort(); } } while (0)
static void fixture_text(KuString text,const char* expected) {
  size_t size=strlen(expected);
  CHECK(text.ptr && text.len==size && !memcmp(text.ptr,expected,size));
  CHECK(text.storage==KU_STRING_OWNED);
}
int main(void) {
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  for (size_t round=0;round<16;round++) {
    size_t calls=ku_perf_calls;
    KuResult_int integer=IntPath(); CHECK(integer.ok && integer.value==99);
    ku_result_drop_int(&integer);
    CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && ku_perf_calls>calls);
    calls=ku_perf_calls;
    KuResult_str string=StrPath(); CHECK(string.ok); fixture_text(string.value,"handled-str");
    ku_result_drop_str(&string);
    CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && ku_perf_calls>calls);
    calls=ku_perf_calls;
    KuResult_array_str array=ArrayPath(); CHECK(array.ok && array.value.len==1u && array.value.data);
    fixture_text(array.value.data[0],"handled-array");
    ku_result_drop_array_str(&array);
    CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && ku_perf_calls>calls);
    CHECK(!ku_perf_overflow && !__ku_call_depth && !__ku_handler_unwind_depth);
  }
  return 0;
}
"#;
