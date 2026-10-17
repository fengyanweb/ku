//! Small source must be refused during synchronous finally expansion, before
//! optimization/C emission; the child has a hard deadline and bounded output.
#[allow(dead_code)]
#[path = "support/bounded_process.rs"]
mod bounded_process;

use bounded_process::{run_bounded, OutputLimits};
use ku::{checker::Checker, ir, lexer::Lexer, parser::Parser};
use std::{process::Command, time::Duration};

const CHILD_ENV: &str = "KU_TEST_SYNC_IR_EXPANSION_CHILD";
const CHILD_NAME: &str = "native_ir_lowering_budget_child";

#[test]
fn native_ir_lowering_budget_child() {
    if std::env::var(CHILD_ENV).as_deref() != Ok("nested-finally") {
        return;
    }
    // Run this default-limit case only on the new implementation. A process
    // watchdog is not a memory limit and is not a safe old-code red strategy.
    // 10 nested finally *bodies*, not nested try bodies. Old lowering copies
    // the leaf 3^10 times before its late size check; the new path stops while
    // reserving its 10,001st block. Never build giant source or run its code.
    let mut body = "println(1)".to_owned();
    for _ in 0..10 {
        body = format!("try {{}} finally {{ {body} }}");
    }
    let source = format!("fn main(): null! {{ {body} return ok(null) }}");
    assert!(source.len() < 1024);
    let ast = Parser::new(Lexer::new(&source).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).expect("compact source is legal");
    let error = ir::lower_program(&ast).expect_err("refuse before large IR is constructed");
    assert!(
        error
            .message
            .contains("IR lowering limit: function block count"),
        "{error}"
    );
    println!("bounded-sync-ir-refusal");
}

#[test]
fn native_ir_lowering_budget_small_source_is_refused_in_bounded_process() {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", CHILD_NAME, "--nocapture"])
        .env(CHILD_ENV, "nested-finally");
    let output = run_bounded(
        &mut command,
        Duration::from_secs(20),
        OutputLimits::new(16 * 1024, 32 * 1024),
    )
    .expect("lowering must finish itself; watchdog termination is failure");
    assert!(
        output.status.success(),
        "{:?}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("bounded-sync-ir-refusal"));
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
