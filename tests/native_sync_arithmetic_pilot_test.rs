//! Synchronous checked arithmetic pilots: actual source-to-C execution, not a
//! direct helper test. The frozen private mailbox names are integration inputs.
//! Never run this against the old raw-C division path: generation must first
//! prove that the checked division helper is both emitted and actually called.
#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use ku::{backend::c, checker::Checker, ir, lexer::Lexer, parser::Parser};
use native_allocation_harness::ALLOCATION_HOOK;
use native_harness::{compile_harness, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};
use std::{fs, process::Command};

// Both functions are non-Result. An arithmetic failure is not an ordinary Fail,
// cannot enter either catch, and does not execute ordinary user finally bodies.
const ORDINARY_SOURCE: &str = r#"
fn Div(n: int, d: int): int {
    owner = "div-" + "owner"
    println(owner)
    try {
        value = n / d
        println("BAD-div-after")
        return value
    } catch (e) { println("BAD-div-catch") }
    finally { println("BAD-div-finally") }
    return 99
}
fn main() {
    owner = "main-" + "owner"
    println(owner)
    try {
        value = Div(7, 0)
        println("BAD-main-after")
    } catch (e) { println("BAD-main-catch") }
    finally { println("BAD-main-finally") }
}
"#;

// Witness C from the cleanup-signal design, with real owners and an explicit
// caller catch that rejects treating a failed callee's zero Result as a Fail.
const CLEANUP_SOURCE: &str = r#"
fn healthy() {
    owner = "healthy-" + "owner"
    println(owner)
    println("healthy")
}
fn broken(n: int): int! {
    owner = "broken-" + "owner"
    println(owner)
    try {
        try { return ok(n / 0) }
        finally { println("broken-inner") fail "suppressed" }
    } catch (e) { println("BAD-broken-catch") return ok(99) }
    finally { healthy() println("broken-outer") }
    return ok(0)
}
fn request(): int! {
    owner = "request-" + "owner"
    println(owner)
    try { while (true) {} }
    finally {
        try {
            value = broken(7)?
            println("BAD-after-call")
        } catch (e) { println("BAD-request-catch") }
        finally { healthy() println("request-outer") }
        println("BAD-after-cleanup")
    }
    return ok(0)
}
fn main() {}
"#;

fn replace_once(source: String, anchor: &str, replacement: &str) -> String {
    assert_eq!(
        source.matches(anchor).count(),
        1,
        "fixture anchor: {anchor}"
    );
    source.replacen(anchor, replacement, 1)
}

fn generate(source: &str) -> String {
    let ast = Parser::new(Lexer::new(source).lex().expect("pilot source lexes"))
        .parse_program()
        .expect("pilot source parses");
    Checker::new()
        .check(&ast)
        .unwrap_or_else(|error| panic!("{error}\n{source}"));
    let lowered = ir::lower_program(&ast).expect("pilot lowers within existing budget");
    let generated = c::generate_c_source(&ir::optimize_program(&lowered)).expect("native C emits");
    assert_eq!(
        generated.matches("static uint32_t ku_int_div(").count(),
        1,
        "do not compile or run the old undefined raw-C division path"
    );
    assert!(
        generated.matches("ku_int_div(").count() > 1,
        "the actual synchronous function must call the checked helper"
    );
    assert!(generated.contains("KuSyncExitSignal"));
    generated
}

// The true generated root remains intact except for its function name. The
// observer calls it and verifies it returned normally after consuming the
// signal and releasing all generated frames. Helper-level exit cannot pass.
const ORDINARY_MAIN: &str = r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr,"ordinary arithmetic pilot line %d: %s\n",__LINE__,#c); abort(); } } while (0)
int main(void) {
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  size_t before=ku_perf_calls;
  int status=fixture_generated_main();
  CHECK(status==1);
  CHECK(ku_perf_calls-before==2);
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  CHECK(!__ku_call_depth && !__ku_handler_unwind_depth);
  CHECK(!__ku_handler_timed_out && !__ku_handler_deadline && !__ku_handler_cleanup_deadline);
  KuSyncExitSignal signal=__ku_sync_take();
  CHECK(signal.kind==KU_SYNC_EXIT_NONE);
  CHECK(__ku_sync_return_signal.kind==KU_SYNC_EXIT_NONE);
  fputs("ordinary-arithmetic-root-ok\n",stdout);
  return 0;
}
"#;

const OBSERVER_GLOBALS: &str = r#"
static unsigned fixture_timed,fixture_request_seen,fixture_broken_seen;
static unsigned fixture_broken_inner_seen,fixture_broken_outer_seen;
static unsigned fixture_healthy_seen,fixture_request_outer_seen;
static unsigned fixture_clock_reads,fixture_selection_reads,fixture_grace_reads;
static char fixture_trace[512];
static size_t fixture_trace_len;
static void fixture_observe_phase(unsigned phase);
"#;

// Read-only with respect to the runtime and mailbox. This observer deliberately
// does not take/reset the signal: healthy helpers themselves must see NONE.
const OBSERVE_PRINT: &str = r#"
  if (stream == stdout) {
    if (fixture_trace_len > sizeof(fixture_trace)-2 ||
        value.len > sizeof(fixture_trace)-fixture_trace_len-2) {
      fputs("arithmetic cleanup trace overflow\n",stderr); abort();
    }
    if (value.len) memcpy(fixture_trace+fixture_trace_len,value.ptr,value.len);
    fixture_trace_len+=value.len;
    fixture_trace[fixture_trace_len++]='|';
    fixture_trace[fixture_trace_len]=0;
    if (value.len==13 && !memcmp(value.ptr,"request-owner",13)) fixture_observe_phase(1);
    else if (value.len==12 && !memcmp(value.ptr,"broken-owner",12)) fixture_observe_phase(2);
    else if (value.len==12 && !memcmp(value.ptr,"broken-inner",12)) fixture_observe_phase(3);
    else if (value.len==7 && !memcmp(value.ptr,"healthy",7)) fixture_observe_phase(4);
    else if (value.len==12 && !memcmp(value.ptr,"broken-outer",12)) fixture_observe_phase(5);
    else if (value.len==13 && !memcmp(value.ptr,"request-outer",13)) fixture_observe_phase(6);
  }
"#;

// The request-owner observation occurs before the real while-loop Safepoint.
// That poll first sees 101; the real timeout entry latches D=1101. All cleanup
// clocks subsequently return 600 so healthy cleanup is permitted under that D.
const CLOCK: &str = r#"
static unsigned long long __ku_handler_now_ms(void) {
  if (++fixture_clock_reads>256) {
    fputs("arithmetic cleanup progress bound exceeded\n",stderr); abort();
  }
  if (__ku_handler_cleanup_deadline) {
    if (__ku_handler_cleanup_deadline!=1101) {
      fputs("arithmetic cleanup deadline renewed\n",stderr); abort();
    }
    fixture_grace_reads++;
    return 600;
  }
  if (fixture_timed && fixture_request_seen) {
    if (!__ku_handler_timed_out) fixture_selection_reads++;
    return 101;
  }
  return 100;
}
"#;

const CLEANUP_MAIN: &str = r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr,"arithmetic cleanup pilot line %d: %s\n",__LINE__,#c); abort(); } } while (0)
static int fixture_empty_string(KuString value) {
  return !value.ptr && !value.len && !value.capacity && !value.storage;
}
static void fixture_empty_error(KuError error) {
  CHECK(fixture_empty_string(error.domain));
  CHECK(fixture_empty_string(error.code));
  CHECK(fixture_empty_string(error.message));
}
static void fixture_observe_phase(unsigned phase) {
  CHECK(!ku_perf_overflow);
  CHECK(__ku_sync_return_signal.kind==KU_SYNC_EXIT_NONE);
  if (phase==1) {
    CHECK(fixture_timed && !fixture_request_seen);
    CHECK(!__ku_handler_timed_out && !__ku_handler_cleanup_deadline);
    CHECK(!__ku_handler_unwind_depth && __ku_handler_deadline==101);
    CHECK(ku_perf_live_allocations==1);
    fixture_request_seen++;
    return;
  }
  if (fixture_timed) {
    CHECK(__ku_handler_timed_out && __ku_handler_unwind_depth>0);
    CHECK(__ku_handler_deadline==101 && __ku_handler_cleanup_deadline==1101);
    CHECK(fixture_request_seen==1);
  } else {
    CHECK(phase==4);
    CHECK(!__ku_handler_timed_out && !__ku_handler_unwind_depth);
    CHECK(!__ku_handler_deadline && !__ku_handler_cleanup_deadline);
  }
  switch (phase) {
    case 2:
      CHECK(!fixture_broken_seen);
      CHECK(ku_perf_live_allocations==2);
      fixture_broken_seen++;
      break;
    case 3:
      CHECK(fixture_broken_seen==1 && !fixture_broken_inner_seen);
      fixture_broken_inner_seen++;
      break;
    case 4:
      if (fixture_timed) CHECK(fixture_broken_inner_seen==1 && fixture_healthy_seen<2);
      else CHECK(!fixture_healthy_seen);
      fixture_healthy_seen++;
      break;
    case 5:
      CHECK(fixture_broken_inner_seen==1 && fixture_healthy_seen==1 && !fixture_broken_outer_seen);
      fixture_broken_outer_seen++;
      break;
    case 6:
      CHECK(fixture_broken_outer_seen==1 && fixture_healthy_seen==2 && !fixture_request_outer_seen);
      fixture_request_outer_seen++;
      break;
    default: CHECK(0);
  }
}
static void fixture_begin(int timed) {
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  CHECK(!__ku_call_depth && !__ku_handler_unwind_depth);
  CHECK(!__ku_handler_timed_out && !__ku_handler_deadline && !__ku_handler_cleanup_deadline);
  // This is a true execution-root boundary, not a nested helper clearing an
  // unconsumed error. Assert it is already empty before exercising root reset.
  CHECK(__ku_sync_return_signal.kind==KU_SYNC_EXIT_NONE);
  __ku_sync_reset();
  fixture_timed=(unsigned)timed;
  fixture_request_seen=fixture_broken_seen=fixture_broken_inner_seen=0;
  fixture_broken_outer_seen=fixture_healthy_seen=fixture_request_outer_seen=0;
  fixture_clock_reads=fixture_selection_reads=fixture_grace_reads=0;
  fixture_trace_len=0; fixture_trace[0]=0;
  if (timed) __ku_handler_timeout_begin(1);
}
static void fixture_timed_case(void) {
  fixture_begin(1);
  size_t before=ku_perf_calls;
  KuResult_int result=request();
  CHECK(!strcmp(fixture_trace,"request-owner|broken-owner|broken-inner|healthy-owner|healthy|broken-outer|healthy-owner|healthy|request-outer|"));
  CHECK(fixture_request_seen==1 && fixture_broken_seen==1 && fixture_broken_inner_seen==1);
  CHECK(fixture_broken_outer_seen==1 && fixture_healthy_seen==2 && fixture_request_outer_seen==1);
  CHECK(fixture_selection_reads==1 && fixture_grace_reads>0);
  CHECK(!__ku_call_depth && !__ku_handler_unwind_depth);
  CHECK(__ku_handler_timed_out && __ku_handler_deadline==101 && __ku_handler_cleanup_deadline==1101);
  CHECK(ku_perf_calls-before==4);
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  // Consume the real callee-to-root signal before interpreting or dropping its
  // internal zero transport Result, and before resetting the original timeout.
  KuSyncExitSignal signal=__ku_sync_take();
  CHECK(signal.kind==KU_SYNC_EXIT_CLEANUP_ABORT && signal.arithmetic_status==KU_INT_DIV_ZERO);
  CHECK(!strcmp(__ku_sync_error_message(signal),"division by zero"));
  CHECK(__ku_sync_return_signal.kind==KU_SYNC_EXIT_NONE);
  CHECK(__ku_sync_take().kind==KU_SYNC_EXIT_NONE);
  CHECK(__ku_handler_timed_out && __ku_handler_cleanup_deadline==1101);
  CHECK(!result.ok && result.value==0);
  fixture_empty_error(result.error);
  ku_result_drop_int(&result);
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  CHECK(__ku_handler_timeout_finish()==1);
  CHECK(!__ku_handler_timed_out && !__ku_handler_deadline && !__ku_handler_cleanup_deadline);
}
static void fixture_healthy_case(void) {
  fixture_begin(0);
  size_t before=ku_perf_calls;
  healthy();
  CHECK(!strcmp(fixture_trace,"healthy-owner|healthy|"));
  CHECK(fixture_healthy_seen==1 && ku_perf_calls-before==1);
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  CHECK(!__ku_call_depth && !__ku_handler_unwind_depth);
  CHECK(__ku_sync_take().kind==KU_SYNC_EXIT_NONE);
  CHECK(__ku_handler_timeout_finish()==0);
}
int main(void) {
  for (unsigned round=0; round<8; ++round) {
    fixture_timed_case();
    fixture_healthy_case();
  }
  fputs("arithmetic-cleanup-pilot-ok\n",stdout);
  return 0;
}
"#;

fn execute(label: &str, generated: String, main: &str, expected_out: &str, expected_err: &str) {
    let directory = TempDir::new(label);
    let path = directory.path().join("program.c");
    fs::write(&path, format!("{generated}\n{main}")).unwrap();
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
    .expect("arithmetic pilot obeys the real process watchdog");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        expected_out
    );
    assert_eq!(
        String::from_utf8(output.stderr).unwrap().replace('\r', ""),
        expected_err
    );
}

#[test]
fn native_sync_integer_fatal_returns_through_real_root_without_user_finally() {
    let generated = replace_once(
        generate(ORDINARY_SOURCE),
        "typedef struct KuString {",
        &format!("{ALLOCATION_HOOK}\ntypedef struct KuString {{"),
    );
    let generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_generated_main(void) {",
    );
    execute(
        "native-sync-arithmetic-ordinary-pilot",
        generated,
        ORDINARY_MAIN,
        "main-owner\ndiv-owner\nordinary-arithmetic-root-ok\n",
        "division by zero\n",
    );
}

#[test]
fn native_sync_integer_cleanup_signal_crosses_callees_without_poisoning_healthy_cleanup() {
    let generated = replace_once(
        generate(CLEANUP_SOURCE),
        "typedef struct KuString {",
        &format!("{ALLOCATION_HOOK}\n{OBSERVER_GLOBALS}\ntypedef struct KuString {{"),
    );
    let generated = replace_once(
        generated,
        "static void ku_string_write(FILE* stream, KuString value) {",
        &format!("static void ku_string_write(FILE* stream, KuString value) {{{OBSERVE_PRINT}"),
    );
    let start_marker = "static unsigned long long __ku_handler_now_ms(void) {";
    let end_marker = "static void __ku_handler_timeout_begin(";
    assert_eq!(generated.matches(start_marker).count(), 1);
    assert_eq!(generated.matches(end_marker).count(), 1);
    let start = generated.find(start_marker).unwrap();
    let end = generated.find(end_marker).unwrap();
    assert!(start < end);
    let generated = format!("{}{CLOCK}{}", &generated[..start], &generated[end..]);
    let generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_unused_source_main(void) {",
    );
    let round = concat!(
        "request-owner\nbroken-owner\nbroken-inner\nhealthy-owner\nhealthy\nbroken-outer\n",
        "healthy-owner\nhealthy\nrequest-outer\nhealthy-owner\nhealthy\n",
    );
    execute(
        "native-sync-arithmetic-cleanup-pilot",
        generated,
        CLEANUP_MAIN,
        &(round.repeat(8) + "arithmetic-cleanup-pilot-ok\n"),
        "",
    );
}

// Keep the original same-frame suppression witness above unchanged. This
// additional variant executes a different second arithmetic failure in that
// frame, after a healthy helper; the first error must remain authoritative.
const MIXED_STATUS_GLOBALS: &str = r#"
static unsigned fixture_div_zero_seen,fixture_overflow_seen;
static void fixture_observe_math(uint32_t status);
"#;

const MIXED_STATUS_OBSERVER: &str = r#"
static void fixture_observe_math(uint32_t status) {
  // Observation only: never manufacture, consume, or reset a runtime signal.
  // This runs at the real raise_math entry after the actual checked operation.
  CHECK(fixture_timed && fixture_request_seen==1 && fixture_broken_seen==1);
  CHECK(__ku_sync_return_signal.kind==KU_SYNC_EXIT_NONE);
  CHECK(__ku_handler_timed_out && __ku_handler_unwind_depth>0);
  CHECK(__ku_handler_deadline==101 && __ku_handler_cleanup_deadline==1101);
  CHECK(__ku_call_depth==2 && ku_perf_live_allocations==2 && !ku_perf_overflow);
  if (status==KU_INT_DIV_ZERO) {
    CHECK(!fixture_div_zero_seen && !fixture_overflow_seen);
    CHECK(!fixture_broken_inner_seen && !fixture_broken_outer_seen && !fixture_healthy_seen);
    fixture_div_zero_seen++;
  } else {
    CHECK(status==KU_INT_OVERFLOW);
    CHECK(fixture_div_zero_seen==1 && !fixture_overflow_seen);
    CHECK(fixture_broken_inner_seen==1 && fixture_broken_outer_seen==1 && fixture_healthy_seen==1);
    fixture_overflow_seen++;
  }
}
"#;

#[test]
fn native_sync_cleanup_preserves_first_division_error_after_later_overflow() {
    // n remains an actual function parameter (7), not a folded constant. Both
    // operations belong to broken(), with healthy() between their two guards.
    let source = replace_once(
        CLEANUP_SOURCE.to_owned(),
        "finally { healthy() println(\"broken-outer\") }",
        "finally { healthy() println(\"broken-outer\") return ok(n + 9223372036854775807) }",
    );
    let generated = generate(&source);
    assert_eq!(generated.matches("static uint32_t ku_int_add(").count(), 1);
    assert!(
        generated.matches("ku_int_add(").count() > 1,
        "the mixed-error variant must actually call checked addition"
    );
    let generated = replace_once(
        generated,
        "typedef struct KuString {",
        &format!(
            "{ALLOCATION_HOOK}\n{OBSERVER_GLOBALS}\n{MIXED_STATUS_GLOBALS}\ntypedef struct KuString {{"
        ),
    );
    let generated = replace_once(
        generated,
        "static void ku_string_write(FILE* stream, KuString value) {",
        &format!("static void ku_string_write(FILE* stream, KuString value) {{{OBSERVE_PRINT}"),
    );
    let generated = replace_once(
        generated,
        "static void __ku_sync_raise_math(uint32_t status) {",
        "static void __ku_sync_raise_math(uint32_t status) {\n  fixture_observe_math(status);",
    );
    let start_marker = "static unsigned long long __ku_handler_now_ms(void) {";
    let end_marker = "static void __ku_handler_timeout_begin(";
    assert_eq!(generated.matches(start_marker).count(), 1);
    assert_eq!(generated.matches(end_marker).count(), 1);
    let start = generated.find(start_marker).unwrap();
    let end = generated.find(end_marker).unwrap();
    assert!(start < end);
    let generated = format!("{}{CLOCK}{}", &generated[..start], &generated[end..]);
    let generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_unused_source_main(void) {",
    );
    let main = replace_once(
        CLEANUP_MAIN.to_owned(),
        "fixture_trace_len=0; fixture_trace[0]=0;",
        "fixture_trace_len=0; fixture_trace[0]=0;\n  fixture_div_zero_seen=fixture_overflow_seen=0;",
    );
    let main = replace_once(
        main,
        "CHECK(ku_perf_calls-before==4);",
        "CHECK(ku_perf_calls-before==4);\n  CHECK(fixture_div_zero_seen==1 && fixture_overflow_seen==1);",
    );
    let main = format!("{main}\n{MIXED_STATUS_OBSERVER}");
    let round = concat!(
        "request-owner\nbroken-owner\nbroken-inner\nhealthy-owner\nhealthy\nbroken-outer\n",
        "healthy-owner\nhealthy\nrequest-outer\nhealthy-owner\nhealthy\n",
    );
    execute(
        "native-sync-arithmetic-first-status",
        generated,
        &main,
        &(round.repeat(8) + "arithmetic-cleanup-pilot-ok\n"),
        "",
    );
}
