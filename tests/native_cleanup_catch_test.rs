//! Cancellation already selected: a failure escaping its attempted finally
//! cannot enter a surrounding ordinary catch. The clock is the only fake.
#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;
use ku::{backend::c, checker::Checker, ir, lexer::Lexer, parser::Parser};
use native_allocation_harness::ALLOCATION_HOOK;
use native_harness::{compile_harness, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};
use std::{fs, process::Command};

const SOURCE: &str = r#"
import fs from "std.fs"

fn timed(): int! {
    owned = "before-" + "owner"
    try {
        try { while (true) {} }
        finally { println("inner") fail "ignored-" + "cleanup" }
    } catch (e) { println("BAD") return ok(99) }
    finally { println("outer") }
    return ok(7)
}
fn local_timed(): int! {
    try { while (true) {} }
    finally {
        try {
            try { return ok(7) } finally { fail "locally-" + "handled" }
        } catch (e) { println("local") }
        println("after-local")
    }
    return ok(7)
}
fn Finish(path: str, content: str): null! {
    fs.write(path, content)?
    return ok(null)
}
fn spin(): int {
    while (true) {}
    return 0
}
fn finite_timed(): null! {
    owned = "owned-" + "payload"
    try {
        spin()
    } finally {
        try {
            Finish("finally-one.txt", owned.clone())?
            fs.write("finally-two.txt", "after-helper")?
        } catch (err) {
            panic("finally write failed: " + err.message)
        }
    }
    return ok(null)
}
fn main() {}
"#;

fn replace_once(source: String, anchor: &str, replacement: &str) -> String {
    assert_eq!(
        source.matches(anchor).count(),
        1,
        "clock/observer anchor: {anchor}"
    );
    source.replacen(anchor, replacement, 1)
}

const OBSERVE_PRINT: &str = r#"
  if (stream == stdout && value.ptr) {
    if (value.len == 3 && !memcmp(value.ptr,"BAD",3)) fixture_bad++;
    if (value.len == 5 && !memcmp(value.ptr,"inner",5)) fixture_inner++;
    if (value.len == 5 && !memcmp(value.ptr,"outer",5)) fixture_outer++;
    if (value.len == 5 && !memcmp(value.ptr,"local",5)) fixture_local++;
    if (value.len == 11 && !memcmp(value.ptr,"after-local",11)) fixture_after_local++;
  }
"#;

const C_MAIN: &str = r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr,"cleanup catch check line %d: %s\n",__LINE__,#c); abort(); } } while (0)
static void fixture_finish_enter(void) {
  CHECK(fixture_finish_calls==0 && fixture_now==200);
  CHECK(__ku_handler_timed_out && __ku_handler_unwind_depth>0);
  CHECK(__ku_handler_deadline==200 && __ku_handler_cleanup_deadline==1200);
  fixture_finish_calls++;
  /* Advance only the clock, within the original D. A helper must not renew D
   * to 1600, and its caller's real post-call poll must let the next write run. */
  fixture_now=600;
}
int main(void) {
  for (unsigned round=0; round<16; ++round) {
    size_t before=ku_perf_calls;
    fixture_now=100; __ku_handler_timeout_begin(1); fixture_now=101;
    KuResult_int first=timed();
    ku_result_drop_int(&first);
    CHECK(fixture_bad==0);
    CHECK(fixture_inner==round+1 && fixture_outer==round+1);
    CHECK(__ku_handler_timed_out && __ku_handler_cleanup_deadline==1101);
    CHECK(__ku_call_depth==0 && __ku_handler_unwind_depth==0);
    CHECK(ku_perf_calls>before && ku_perf_live_allocations==0 && ku_perf_live_bytes==0 && !ku_perf_overflow);
    CHECK(__ku_handler_timeout_finish()==1);
    fixture_now=200; __ku_handler_timeout_begin(1); fixture_now=201;
    KuResult_int second=local_timed();
    ku_result_drop_int(&second);
    CHECK(fixture_bad==0 && fixture_local==round+1 && fixture_after_local==round+1);
    CHECK(__ku_handler_timed_out && __ku_handler_cleanup_deadline==1201);
    CHECK(__ku_call_depth==0 && __ku_handler_unwind_depth==0);
    CHECK(ku_perf_live_allocations==0 && ku_perf_live_bytes==0 && !ku_perf_overflow);
    CHECK(__ku_handler_timeout_finish()==1);
    CHECK(!__ku_handler_deadline && !__ku_handler_cleanup_deadline && !__ku_handler_timed_out && !__ku_handler_unwind_depth);
  }
  size_t before=ku_perf_calls;
  fixture_now=100; __ku_handler_timeout_begin(100); fixture_now=200;
  KuResult_null finite=finite_timed();
  CHECK(!finite.ok);
  ku_result_drop_null(&finite);
  CHECK(fixture_finish_calls==1 && fixture_now==600);
  CHECK(__ku_handler_timed_out && __ku_handler_deadline==200 && __ku_handler_cleanup_deadline==1200);
  CHECK(!__ku_call_depth && !__ku_handler_unwind_depth);
  CHECK(ku_perf_calls>before && !ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  CHECK(__ku_handler_timeout_finish()==1);
  CHECK(!__ku_handler_deadline && !__ku_handler_cleanup_deadline && !__ku_handler_timed_out && !__ku_handler_unwind_depth);
  return 0;
}
"#;

#[test]
fn native_cleanup_failed_finally_skips_outer_catch_but_preserves_local_recovery() {
    let mut ast = Parser::new(Lexer::new(SOURCE).lex().unwrap())
        .parse_program()
        .unwrap();
    // The CLI resolves this standard import before the direct checker runs.
    for item in &mut ast.items {
        if let ku::ast::Item::Import(import) = item {
            assert_eq!(import.path, "std.fs");
            *item = ku::ast::Item::Module(ku::ast::ModuleDecl {
                name: "std:fs".into(),
                span: import.span,
            });
        }
    }
    Checker::new()
        .check(&ast)
        .expect("ordinary typed source is legal");
    let lowered = ir::lower_program(&ast).unwrap();
    let generated = c::generate_c_source(&ir::optimize_program(&lowered)).unwrap();
    let generated = replace_once(generated, "typedef struct KuString {", &format!(
        "{ALLOCATION_HOOK}\nstatic unsigned long long fixture_now=100;\nstatic unsigned fixture_bad,fixture_inner,fixture_outer,fixture_local,fixture_after_local,fixture_finish_calls;\nstatic void fixture_finish_enter(void);\ntypedef struct KuString {{"));
    let generated = replace_once(
        generated,
        "static void ku_string_write(FILE* stream, KuString value) {",
        &format!("static void ku_string_write(FILE* stream, KuString value) {{{OBSERVE_PRINT}"),
    );
    let generated = replace_once(
        generated,
        "KuResult_null Finish(KuString path, KuString content) {",
        "KuResult_null Finish(KuString path, KuString content) {\n  fixture_finish_enter();",
    );
    let start_marker = "static unsigned long long __ku_handler_now_ms(void) {";
    let end_marker = "static void __ku_handler_timeout_begin(";
    assert_eq!(generated.matches(start_marker).count(), 1);
    assert_eq!(generated.matches(end_marker).count(), 1);
    let start = generated.find(start_marker).unwrap();
    let end = generated.find(end_marker).unwrap();
    assert!(start < end);
    let clock = "static unsigned long long __ku_handler_now_ms(void) { return fixture_now; }\n";
    let generated = format!("{}{clock}{}", &generated[..start], &generated[end..]);
    let generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_unused_source_main(void) {",
    );
    let generated = format!("{generated}\n{C_MAIN}");
    let directory = TempDir::new("native-cleanup-catch");
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
    .expect("cleanup signal must not loop or hang");
    assert!(
        output.status.success(),
        "{:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "inner\nouter\nlocal\nafter-local\n".repeat(16)
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // A fresh directory prevents old marker files from satisfying this gate.
    // The real helper and both real writes ran at known virtual times below D;
    // a missing second marker remains a hard failure, independent of host I/O.
    assert_eq!(
        fs::read_to_string(directory.path().join("finally-one.txt")).expect("first finally marker"),
        "owned-payload"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("finally-two.txt"))
            .expect("second finally marker"),
        "after-helper"
    );
}
