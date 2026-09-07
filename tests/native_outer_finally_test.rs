//! A catch-only try must not hide an outer finally from return or timeout.
//! Real source, Checker, synchronous IR and generated C; the C hook changes
//! only the clock source and observes existing allocation/cleanup behavior.
#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use ku::{
    backend::c,
    checker::Checker,
    ir::{self, IrExprKind, IrInst, IrLValue, IrTerminator, IrType},
    lexer::Lexer,
    parser::Parser,
};
use native_allocation_harness::ALLOCATION_HOOK;
use native_harness::{compile_harness, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};
use std::{fs, process::Command};

const MINIMAL: &str = r#"
fn f(): int {
    try {
        try { return 7 } catch (e) {}
        return 8
    } finally { println("outer") }
    return 9
}
fn main(): null! { println(f()) return ok(null) }
"#;

fn checked(source: &str) -> ku::ast::Program {
    let ast = Parser::new(Lexer::new(source).lex().expect("source lexes"))
        .parse_program()
        .expect("source parses");
    Checker::new()
        .check(&ast)
        .unwrap_or_else(|error| panic!("{error}\n{source}"));
    ast
}

#[test]
fn native_outer_finally_return_selects_matching_handler_and_payload_slot_in_ir() {
    let lowered = ir::lower_program(&checked(MINIMAL)).expect("source lowers");
    let function = lowered.functions.iter().find(|f| f.name == "f").unwrap();
    let entry = function.blocks.iter().find(|b| b.id.0 == 0).unwrap();
    let IrTerminator::Jump(target) = entry.terminator else {
        panic!(
            "inner return bypassed outer finally: {:?}",
            entry.terminator
        );
    };
    let cleanup = function.blocks.iter().find(|b| b.id == target).unwrap();
    assert!(cleanup.name.starts_with("finally_return"), "{cleanup:?}");
    let (slot, value) = entry
        .instructions
        .iter()
        .find_map(|instruction| match instruction {
            IrInst::Store {
                target: IrLValue::Local(name),
                value,
            } if matches!(&value.kind, IrExprKind::Literal(text) if text == "7") => {
                Some((name, value))
            }
            _ => None,
        })
        .expect("return payload is saved before finally");
    assert_eq!(value.ty, IrType::Int);
    assert!(matches!(&cleanup.terminator,
        IrTerminator::Return(Some(value))
            if matches!(&value.kind, IrExprKind::Local(name) if name == slot)));
    let optimized = ir::optimize_program(&lowered);
    let function = optimized.functions.iter().find(|f| f.name == "f").unwrap();
    assert!(
        function.blocks.iter().any(|b| b.id == target),
        "optimizer removed required cleanup"
    );
    assert!(!function.blocks.iter().any(|b| matches!(&b.terminator,
        IrTerminator::Return(Some(value))
            if matches!(&value.kind, IrExprKind::Literal(text) if text == "7"))));
}

#[test]
fn native_outer_finally_change_does_not_route_ordinary_fatal_through_finally() {
    let source = "fn main() { try { panic(\"fatal\") } finally { println(\"not-run\") } }";
    let lowered = ir::lower_program(&checked(source)).unwrap();
    let main = lowered.functions.iter().find(|f| f.name == "main").unwrap();
    let entry = main.blocks.iter().find(|b| b.id.0 == 0).unwrap();
    assert_eq!(entry.terminator, IrTerminator::Unreachable);
    assert!(matches!(entry.instructions.last(), Some(IrInst::Panic(_))));
    let optimized = ir::optimize_program(&lowered);
    let main = optimized
        .functions
        .iter()
        .find(|f| f.name == "main")
        .unwrap();
    assert!(!main.blocks.iter().any(|b| b.name.starts_with("finally")));
}

// Every string that matters for lifetime testing is dynamically constructed.
// The test exercises saved return owners and abandoned/overridden owners, not
// just static literals whose drop would hide a missing or repeated release.
const SOURCE: &str = r#"
fn body_copy(): int {
    try {
        try { return 7 } catch (e) {}
        return 8
    } finally { println("body-outer") }
    return 9
}
fn body_owned(name: str): str {
    text = "body-" + name
    try {
        try { return text } catch (e) {}
        return "bad"
    } finally { println("owned-outer") }
    return "bad"
}
fn catch_owned(): str {
    try {
        try { fail "recover" } catch (e) {
            try { return "caught-" + e.message } catch (ignored) {}
        }
        return "bad"
    } finally { println("catch-outer") }
    return "bad"
}
fn nested_owned(): str {
    try {
        try {
            try {
                try { return "deep-" + "value" } catch (e) {}
            } finally { println("nested-inner") }
        } catch (e) {}
        return "bad"
    } finally { println("nested-outer") }
    return "bad"
}
fn override_owned(): str {
    try {
        try { return "discard-" + "one" } catch (e) {}
        return "bad"
    } finally { return "replacement-" + "two" }
    return "bad"
}
fn result_owned(name: str): str! {
    try {
        try { return ok("result-" + name) } catch (e) {}
        return ok("bad")
    } finally { println("result-outer") }
    return ok("bad")
}
fn error_payload(): str! {
    fail { domain: "own-" + "domain", code: "own-" + "code", message: "own-" + "message" }
}
fn result_error(): str! {
    result = error_payload()
    try {
        try { return result } catch (e) {}
        return ok("bad")
    } finally { println("error-outer") }
    return ok("bad")
}
fn body_void() {
    try { try { return } catch (e) {} } finally { println("void-outer") }
}
// Only the C watchdog fixture calls this function. It uses the real generated
// loop Safepoint and timeout-return CFG; no source-level timeout API is added.
fn timed(): int {
    owned = "timer-" + "payload"
    try {
        try { while (true) {} } catch (e) {}
    } finally { println("timeout-outer") }
    return 9
}
fn main(): null! {
    println(body_copy())
    println(body_owned("value"))
    println(catch_owned())
    println(nested_owned())
    println(override_owned())
    println(result_owned("value")?)
    try { unexpected = result_error()? println(unexpected) } catch (e) {
        println(e.domain)
        println(e.code)
        println(e.message)
    }
    body_void()
    return ok(null)
}
"#;

const EXPECTED: &str = concat!(
    "body-outer\n7\nowned-outer\nbody-value\ncatch-outer\ncaught-recover\n",
    "nested-inner\nnested-outer\ndeep-value\nreplacement-two\n",
    "result-outer\nresult-value\nerror-outer\nown-domain\nown-code\nown-message\nvoid-outer\n",
);

fn replace_once(source: String, anchor: &str, replacement: &str) -> String {
    assert_eq!(
        source.matches(anchor).count(),
        1,
        "unique fixture anchor: {anchor}"
    );
    source.replacen(anchor, replacement, 1)
}

const C_MAIN: &str = r#"
int main(void) {
  for (unsigned iteration=0; iteration<32; ++iteration) {
    size_t before=ku_perf_calls;
    if (fixture_source_main()!=0) return 70;
    if (ku_perf_calls<=before || ku_perf_live_allocations || ku_perf_live_bytes || ku_perf_overflow) return 71;
    if (__ku_call_depth || __ku_handler_unwind_depth || __ku_handler_timed_out) return 72;
    fixture_now=100;
    __ku_handler_timeout_begin(1);
    fixture_now=101;
    if (timed()!=0) return 73;
    if (!__ku_handler_timed_out || __ku_handler_cleanup_deadline!=1101) return 74;
    if (__ku_handler_unwind_depth || __ku_call_depth) return 75;
    if (ku_perf_live_allocations || ku_perf_live_bytes || ku_perf_overflow) return 76;
    if (__ku_handler_timeout_finish()!=1) return 77;
    if (__ku_handler_deadline || __ku_handler_cleanup_deadline || __ku_handler_timed_out || __ku_handler_unwind_depth) return 78;
  }
  return 0;
}
"#;

#[test]
fn native_outer_finally_owned_returns_override_and_timeout_execute_without_leaks() {
    let ast = checked(SOURCE);
    let lowered = ir::lower_program(&ast).expect("source lowers");
    let generated =
        c::generate_c_source(&ir::optimize_program(&lowered)).expect("native C generation");
    assert!(!generated.contains("run_source") && !generated.contains("const SOURCE"));
    let generated = replace_once(generated, "typedef struct KuString {",
        &format!("{ALLOCATION_HOOK}\nstatic unsigned long long fixture_now=100;\ntypedef struct KuString {{"));
    let start_marker = "static unsigned long long __ku_handler_now_ms(void) {";
    let end_marker = "static void __ku_handler_timeout_begin(";
    assert_eq!(generated.matches(start_marker).count(), 1);
    assert_eq!(generated.matches(end_marker).count(), 1);
    let start = generated.find(start_marker).unwrap();
    let end = generated.find(end_marker).unwrap();
    assert!(start < end);
    // Fake time, not fake timeout/return/cleanup: the actual emitted poll,
    // enter/leave, IR edges and owned destructors remain unchanged.
    let clock = "static unsigned long long __ku_handler_now_ms(void) { return fixture_now; }\n";
    let generated = format!("{}{clock}{}", &generated[..start], &generated[end..]);
    let generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_source_main(void) {",
    );
    let generated = format!("{generated}\n{C_MAIN}");
    let directory = TempDir::new("native-outer-finally");
    let source_path = directory.path().join("main.ku");
    fs::write(&source_path, SOURCE).unwrap();
    let interpreted = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_ku"))
            .arg("run")
            .arg(&source_path)
            .current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("interpreter completes under the process watchdog");
    assert!(
        interpreted.status.success(),
        "{}",
        String::from_utf8_lossy(&interpreted.stderr)
    );
    assert_eq!(
        String::from_utf8(interpreted.stdout)
            .unwrap()
            .replace('\r', ""),
        EXPECTED
    );
    assert!(
        interpreted.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&interpreted.stderr)
    );
    let c_path = directory.path().join("program.c");
    fs::write(&c_path, generated).unwrap();
    let Some(executable) = compile_harness(directory.path(), &c_path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must execute native finally cleanup"
        );
        return;
    };
    fs::remove_file(c_path).unwrap();
    fs::remove_file(source_path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("native finally/timeout fixture terminates under the process watchdog");
    assert!(
        output.status.success(),
        "{:?}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        format!("{EXPECTED}timeout-outer\n").repeat(32)
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
