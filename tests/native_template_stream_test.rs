//! Streaming template lowering must preserve the existing generated runtime.
//! Modest legal input only: no old-code stack/memory exhaustion experiments.
#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use ku::{
    ast::BinaryOp,
    backend::c,
    checker::Checker,
    ir::{self, IrExprKind, IrInst, IrType},
    lexer::Lexer,
    parser::Parser,
};
use native_allocation_harness::ALLOCATION_HOOK;
use native_harness::{compile_harness, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};
use std::{fs, process::Command};

const PARTS: usize = 96;
const SOURCE: &str = r#"
fn EmptyText(): str { return `` }
fn StaticText(): str { return `plain \{x\}` }
fn Many(): str { return `__MANY__` }
fn Piece(label: str): str { println(label) return label + "-owned" }
fn Read(&value: str): str { println(value) return value.clone() }
fn FailRead(&value: str): str! {
    println(value)
    fail { domain: "template-" + "domain", code: "template-" + "code", message: "expected-" + "failure" }
}
fn Build(): str { return `pre{Piece("a")}mid{Read("borrow-" + "ok")}post{Piece("b")}` }
fn Reject(): str! {
    return ok(`prefix{Piece("first")}middle{FailRead("borrow-" + "fail")?}{Piece("BAD")}`)
}
// Called only by the C clock fixture, after normal interpreter/native parity.
fn TimerRoot(): str { return "timer-" + "root" }
fn TimerRead(&value: str): str { return value.clone() }
fn TimedTemplate(): str {
    return `{"held-" + "prefix"}text{TimerRead(TimerRoot())}suffix{Piece("BAD")}`
}
fn main(): null! {
    println(EmptyText())
    println(StaticText())
    println(Many())
    println(`你好{7}世界`)
    println(Build())
    try { value = Reject()? println("BAD returned") println(value) } catch (err) {
        println(err.domain) println(err.code) println(err.message)
    }
    return ok(null)
}
"#;

fn source() -> String {
    assert_eq!(SOURCE.matches("__MANY__").count(), 1);
    SOURCE.replacen("__MANY__", &"{1}".repeat(PARTS), 1)
}

fn lowered(source: &str) -> ir::IrProgram {
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new()
        .check(&ast)
        .unwrap_or_else(|error| panic!("{error}\n{source}"));
    ir::lower_program(&ast).unwrap()
}

fn replace_once(source: String, anchor: &str, replacement: &str) -> String {
    assert_eq!(source.matches(anchor).count(), 1, "unique anchor: {anchor}");
    source.replacen(anchor, replacement, 1)
}

#[test]
fn native_template_many_parts_keep_concat_operands_in_flat_temporaries() {
    let program = lowered(&source());
    let function = program.functions.iter().find(|f| f.name == "Many").unwrap();
    let mut additions = 0;
    for block in &function.blocks {
        for instruction in &block.instructions {
            let IrInst::Temp { value, .. } = instruction else {
                continue;
            };
            let IrExprKind::Binary { left, op, right } = &value.kind else {
                continue;
            };
            assert_eq!(*op, BinaryOp::Add);
            assert_eq!(value.ty, IrType::Str);
            assert!(matches!(left.kind, IrExprKind::Temp(_)));
            assert!(matches!(right.kind, IrExprKind::Temp(_)));
            additions += 1;
        }
    }
    assert_eq!(additions, PARTS - 1);
}

#[test]
fn native_template_stream_matches_interpreter_and_reclaims_concat_and_borrow_owners() {
    let source = source();
    let lowered = lowered(&source);
    let generated = c::generate_c_source(&ir::optimize_program(&lowered)).unwrap();
    assert!(!generated.contains("run_source") && !generated.contains("const SOURCE"));
    let generated = replace_once(
        generated,
        "typedef struct KuString {",
        &format!("{ALLOCATION_HOOK}\nstatic unsigned long long fixture_now=100;\nstatic size_t fixture_root_calls=0,fixture_read_calls=0;\ntypedef struct KuString {{"),
    );
    // A function boundary advances only the clock. The real root-returning
    // call, deferred post-call poll, registration and drops are not replaced.
    let clock_start_marker = "static unsigned long long __ku_handler_now_ms(void) {";
    let clock_end_marker = "static void __ku_handler_timeout_begin(";
    assert_eq!(generated.matches(clock_start_marker).count(), 1);
    assert_eq!(generated.matches(clock_end_marker).count(), 1);
    let clock_start = generated.find(clock_start_marker).unwrap();
    let clock_end = generated.find(clock_end_marker).unwrap();
    assert!(clock_start < clock_end);
    let generated = format!(
        "{}static unsigned long long __ku_handler_now_ms(void) {{ return fixture_now; }}\n{}",
        &generated[..clock_start],
        &generated[clock_end..],
    );
    let generated = replace_once(
        generated,
        "KuString TimerRoot(void) {",
        "KuString TimerRoot(void) {\n  if (!ku_perf_live_allocations) abort();\n  fixture_root_calls++; fixture_now=101;",
    );
    let generated = replace_once(
        generated,
        "KuString TimerRead(const KuString* value) {",
        "KuString TimerRead(const KuString* value) {\n  fixture_read_calls++;",
    );
    let generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_source_main(void) {",
    );
    let generated = format!(
        "{generated}\n{}",
        C_MAIN.replace("__PARTS__", &PARTS.to_string())
    );
    let directory = TempDir::new("native-template-stream");
    let source_path = directory.path().join("main.ku");
    fs::write(&source_path, &source).unwrap();
    let interpreted = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_ku"))
            .arg("run")
            .arg(&source_path)
            .current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("modest source interpreter run obeys the real watchdog");
    assert!(
        interpreted.status.success(),
        "{}",
        String::from_utf8_lossy(&interpreted.stderr)
    );
    assert!(interpreted.stderr.is_empty());
    let expected = format!(
        "\nplain {{x}}\n{}\n你好7世界\na\nborrow-ok\nb\nprea-ownedmidborrow-okpostb-owned\nfirst\nborrow-fail\ntemplate-domain\ntemplate-code\nexpected-failure\n",
        "1".repeat(PARTS),
    );
    assert_eq!(
        String::from_utf8(interpreted.stdout)
            .unwrap()
            .replace('\r', ""),
        expected,
    );
    let c_path = directory.path().join("program.c");
    fs::write(&c_path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &c_path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must compile and execute the actual native template"
        );
        return;
    };
    fs::remove_file(source_path).unwrap();
    fs::remove_file(c_path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("finite native template rounds obey the real watchdog");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        expected.repeat(8),
    );
}

const C_MAIN: &str = r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr,"template stream check line %d: %s\n",__LINE__,#c); abort(); } } while (0)
static void fixture_zero(void) {
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  CHECK(!__ku_call_depth && !__ku_handler_unwind_depth && !__ku_handler_timed_out);
}
int main(void) {
  fixture_zero();
  size_t calls=ku_perf_calls;
  KuString empty=EmptyText(); CHECK(empty.len==0 && empty.storage==KU_STRING_STATIC);
  ku_string_drop(&empty); CHECK(ku_perf_calls==calls); fixture_zero();
  KuString literal=StaticText();
  CHECK(literal.storage==KU_STRING_STATIC && literal.len==9u && !memcmp(literal.ptr,"plain {x}",9));
  ku_string_drop(&literal); CHECK(ku_perf_calls==calls); fixture_zero();
  KuString many=Many(); CHECK(many.storage==KU_STRING_OWNED && many.len==__PARTS__u);
  for (size_t index=0;index<many.len;index++) CHECK(many.ptr[index]=='1');
  CHECK(ku_perf_calls>calls && ku_perf_live_allocations>0 && ku_perf_live_bytes>0);
  ku_string_drop(&many); fixture_zero();
  for (size_t round=0;round<8;round++) {
    calls=ku_perf_calls;
    CHECK(fixture_source_main()==0);
    CHECK(ku_perf_calls>calls);
    fixture_zero();
  }
  calls=ku_perf_calls;
  __ku_handler_timeout_begin(1);
  CHECK(__ku_handler_deadline==101 && !__ku_handler_cleanup_deadline);
  KuString timed=TimedTemplate();
  CHECK(fixture_root_calls==1 && fixture_read_calls==0 && ku_perf_calls>calls);
  CHECK(timed.ptr==NULL && timed.len==0 && timed.capacity==0);
  ku_string_drop(&timed);
  CHECK(__ku_handler_timed_out && __ku_handler_cleanup_deadline==1101);
  CHECK(!__ku_call_depth && !__ku_handler_unwind_depth);
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  CHECK(__ku_handler_timeout_finish()==1);
  CHECK(!__ku_handler_deadline && !__ku_handler_cleanup_deadline);
  fixture_zero();
  return 0;
}
"#;
