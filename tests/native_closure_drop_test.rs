//! Ordinary fresh borrowed closure cleanup: does not require arithmetic failure
//! or timeout injection. Actual generated frames release captured env/cell owners.
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
fn Make(): fn(): int {
    value = 41
    return () => { return value }
}
fn Use(&callback: fn(): int, n: int): int { return callback() + n }
fn main(): null! {
    println(Use(Make(), 1))
    shared = Make()
    println(Use(shared, 1))
    println(shared())
    return ok(null)
}
"#;

const MAIN: &str = r#"
#undef malloc
#undef calloc
#undef realloc
#undef free
#include <assert.h>
int main(void) {
  for (size_t round = 0; round < 16; round++) {
    assert(!ku_perf_live_allocations && !ku_perf_live_bytes);
    size_t before = ku_perf_calls;
    assert(ku_source_entry() == 0);
    /* Both Make calls must create actual captured environments/cells. */
    assert(ku_perf_calls > before);
    assert(!ku_perf_live_allocations && !ku_perf_live_bytes);
    assert(!ku_perf_overflow && ku_perf_peak_bytes > 0);
  }
  return 0;
}
"#;

#[test]
fn native_closure_fresh_borrow_drop_clears_owner_before_frame_cleanup() {
    let ast = Parser::new(Lexer::new(SOURCE).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new()
        .check(&ast)
        .expect("fresh closure may be borrowed; shared source remains callable");
    let program = ir::optimize_program(&ir::lower_program(&ast).unwrap());
    ir::verify_borrow_contract(&program).unwrap();
    let generated = c::generate_c_source(&program).unwrap();
    assert!(
        !generated.contains("__ku_drop_borrow_temp"),
        "drop must be lowered, not left as an unresolved intrinsic"
    );
    assert!(!generated.contains("run_source"));
    assert_eq!(generated.matches("int main(void)").count(), 1);
    assert_eq!(generated.matches("typedef struct KuString {").count(), 1);
    let generated = generated
        .replacen(
            "typedef struct KuString {",
            &format!("{ALLOCATION_HOOK}\ntypedef struct KuString {{"),
            1,
        )
        .replacen("int main(void)", "int ku_source_entry(void)", 1);
    let directory = TempDir::new("closure-fresh-borrow-drop");
    let ku_path = directory.path().join("main.ku");
    fs::write(&ku_path, SOURCE).unwrap();
    let interpreted = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_ku"))
            .arg("run")
            .arg(&ku_path),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("interpreter completes without a watchdog termination");
    assert!(
        interpreted.status.success(),
        "{}",
        String::from_utf8_lossy(&interpreted.stderr)
    );
    let expected = "42\n42\n41\n";
    assert_eq!(
        String::from_utf8(interpreted.stdout)
            .unwrap()
            .replace('\r', ""),
        expected
    );
    assert!(interpreted.stderr.is_empty());
    let c_path = directory.path().join("program.c");
    fs::write(&c_path, format!("{generated}\n{MAIN}")).unwrap();
    let Some(executable) = compile_harness(directory.path(), &c_path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires real C execution"
        );
        return;
    };
    fs::remove_file(c_path).unwrap();
    fs::remove_file(ku_path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("real closure cleanup must finish under the existing process bound");
    assert!(
        output.status.success(),
        "{:?}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        expected.repeat(16)
    );
    assert!(output.stderr.is_empty());
}
