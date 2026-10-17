//! Two real synchronous Ku roots retain different arithmetic exit signals at
//! the same time. Clock and event observers control only time and scheduling.
//! No shared allocation ledger: source values are Copy or static string data.
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use ku::{backend::c, checker::Checker, ir, lexer::Lexer, parser::Parser};
use native_harness::{
    compile_harness, run_bounded, TempDir, NATIVE_THREAD_LIFECYCLE_HARNESS, RUN_LIMITS, RUN_TIMEOUT,
};
use std::{fs, process::Command};

const SOURCE: &str = r#"
fn Overflow(value: int): int { return value + 1 }
fn Divide(left: int, right: int): int { return left / right }
fn Healthy(value: int): int { return value + 1 }
fn TimedRoot(): int! {
    try { while (true) {} }
    finally {
        println("A-cleanup")
        value = Overflow(9223372036854775807)
        println("BAD-A")
    }
    return ok(0)
}
fn FatalRoot(): int {
    println("B-enter")
    value = Divide(7, 0)
    println("BAD-B")
    return value
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

// Inserted after KU_THREAD_LOCAL is defined, before the runtime clock. Only
// events are shared between threads; each observer/clock counter is thread-local.
const FIXTURE_GLOBALS: &str = r#"
enum { FIXTURE_A_CLEANUP, FIXTURE_B_PENDING, FIXTURE_A_RETURNED,
       FIXTURE_B_TAKEN, FIXTURE_A_TAKEN, FIXTURE_EVENT_COUNT };
enum { FIXTURE_WAIT_MS=3000, FIXTURE_JOIN_MS=5000 };
typedef struct FixtureRound { KuTestEvent events[FIXTURE_EVENT_COUNT]; } FixtureRound;
static KU_THREAD_LOCAL FixtureRound* fixture_context;
static KU_THREAD_LOCAL unsigned fixture_role,fixture_clock_phase,fixture_clock_reads;
static KU_THREAD_LOCAL unsigned fixture_cleanup_seen,fixture_enter_seen;
static KU_THREAD_LOCAL int fixture_failed;
"#;

// This code runs at the START of the real string write. A blocks before its
// bytes are written; B_PENDING is set only after FatalRoot (including println's
// newline) returns. Hence successful actual stdout is B-enter then A-cleanup.
const OBSERVE_PRINT: &str = r#"
  if (stream==stdout) {
    unsigned phase=0;
    if (value.len==9 && !memcmp(value.ptr,"A-cleanup",9)) phase=1;
    else if (value.len==7 && !memcmp(value.ptr,"B-enter",7)) phase=2;
    fixture_observe_print(phase);
  }
"#;

const CLOCK: &str = r#"
static unsigned long long __ku_handler_now_ms(void) {
  if (++fixture_clock_reads>128) {
    fputs("TLS arithmetic clock progress bound exceeded\n",stderr); abort();
  }
  if (fixture_role==1) {
    if (__ku_handler_cleanup_deadline) return 600;
    return fixture_clock_phase ? 101 : 100;
  }
  return 9000;
}
"#;

const C_MAIN: &str = r#"
static int fixture_wake_all(FixtureRound* round) {
  int result=1;
  for (unsigned i=0; i<FIXTURE_EVENT_COUNT; ++i) {
    if (!ku_test_event_set(&round->events[i])) result=0;
  }
  return result;
}
static void fixture_fail(FixtureRound* round,int line,const char* expression) {
  fixture_failed=1;
  fprintf(stderr,"TLS arithmetic line %d: %s\n",line,expression);
  // Error-only wake lets the other thread finish its actual source and return.
  // The failing thread's outcome remains nonzero: this is never a pass path.
  if (round) (void)fixture_wake_all(round);
}
#define REQUIRE(c) do { if (!(c)) { fixture_fail(fixture_context,__LINE__,#c); return 1; } } while (0)
#define OBSERVE(c) do { if (!(c)) { fixture_fail(fixture_context,__LINE__,#c); return; } } while (0)
static int fixture_empty_string(KuString value) {
  return !value.ptr && !value.len && !value.capacity && !value.storage;
}
static int fixture_empty_error(KuError error) {
  return fixture_empty_string(error.domain) && fixture_empty_string(error.code) && fixture_empty_string(error.message);
}
static int fixture_runtime_empty(void) {
  return __ku_sync_return_signal.kind==KU_SYNC_EXIT_NONE && !__ku_call_depth &&
      !__ku_handler_timed_out && !__ku_handler_deadline &&
      !__ku_handler_cleanup_deadline && !__ku_handler_unwind_depth;
}
static void fixture_observe_print(unsigned phase) {
  OBSERVE(fixture_context && !fixture_failed);
  OBSERVE(__ku_sync_return_signal.kind==KU_SYNC_EXIT_NONE);
  if (phase==1) {
    OBSERVE(fixture_role==1 && !fixture_cleanup_seen && !fixture_enter_seen);
    OBSERVE(__ku_call_depth==1 && __ku_handler_unwind_depth==1);
    OBSERVE(__ku_handler_timed_out && __ku_handler_deadline==101 && __ku_handler_cleanup_deadline==1101);
    fixture_cleanup_seen=1;
    OBSERVE(ku_test_event_set(&fixture_context->events[FIXTURE_A_CLEANUP]));
    OBSERVE(ku_test_event_wait(&fixture_context->events[FIXTURE_B_PENDING],FIXTURE_WAIT_MS));
    // B has returned with an unconsumed ordinary fatal signal. A still owns its
    // active frame/timeout and has no return signal until its own math executes.
    OBSERVE(__ku_sync_return_signal.kind==KU_SYNC_EXIT_NONE);
    OBSERVE(__ku_call_depth==1 && __ku_handler_unwind_depth==1);
    OBSERVE(__ku_handler_timed_out && __ku_handler_deadline==101 && __ku_handler_cleanup_deadline==1101);
  } else if (phase==2) {
    OBSERVE(fixture_role==2 && !fixture_enter_seen && !fixture_cleanup_seen);
    OBSERVE(__ku_call_depth==1 && !__ku_handler_unwind_depth);
    OBSERVE(!__ku_handler_timed_out && !__ku_handler_deadline && !__ku_handler_cleanup_deadline);
    fixture_enter_seen=1;
  } else OBSERVE(0); /* BAD-A/B or an unexpected source continuation */
}
static int fixture_healthy(void) {
  REQUIRE(fixture_runtime_empty());
  int64_t result=Healthy(41);
  KuSyncExitSignal signal=__ku_sync_take();
  REQUIRE(signal.kind==KU_SYNC_EXIT_NONE);
  REQUIRE(result==42);
  REQUIRE(fixture_runtime_empty());
  return 0;
}
static int fixture_thread_a(void* raw) {
  fixture_context=(FixtureRound*)raw;
  fixture_role=1;
  REQUIRE(fixture_runtime_empty());
  __ku_sync_reset();
  fixture_clock_phase=0;
  __ku_handler_timeout_begin(1);
  REQUIRE(__ku_handler_deadline==101 && !__ku_handler_timed_out);
  fixture_clock_phase=1;
  KuResult_int result=TimedRoot();
  REQUIRE(!fixture_failed && fixture_cleanup_seen==1 && !fixture_enter_seen);
  // Observation only: do not consume this signal until B has consumed its own.
  REQUIRE(__ku_sync_return_signal.kind==KU_SYNC_EXIT_CLEANUP_ABORT);
  REQUIRE(__ku_sync_return_signal.arithmetic_status==KU_INT_OVERFLOW);
  REQUIRE(!__ku_call_depth && !__ku_handler_unwind_depth);
  REQUIRE(__ku_handler_timed_out && __ku_handler_deadline==101 && __ku_handler_cleanup_deadline==1101);
  REQUIRE(ku_test_event_set(&fixture_context->events[FIXTURE_A_RETURNED]));
  REQUIRE(ku_test_event_wait(&fixture_context->events[FIXTURE_B_TAKEN],FIXTURE_WAIT_MS));
  REQUIRE(__ku_sync_return_signal.kind==KU_SYNC_EXIT_CLEANUP_ABORT);
  REQUIRE(__ku_sync_return_signal.arithmetic_status==KU_INT_OVERFLOW);
  REQUIRE(__ku_handler_timed_out && __ku_handler_deadline==101 && __ku_handler_cleanup_deadline==1101);
  KuSyncExitSignal signal=__ku_sync_take();
  REQUIRE(signal.kind==KU_SYNC_EXIT_CLEANUP_ABORT && signal.arithmetic_status==KU_INT_OVERFLOW);
  REQUIRE(!strcmp(__ku_sync_error_message(signal),"integer overflow"));
  // Payload inspection is after root take, never treating the dummy as Err.
  REQUIRE(!result.ok && result.value==0 && fixture_empty_error(result.error));
  ku_result_drop_int(&result);
  REQUIRE(__ku_handler_timeout_finish()==1);
  REQUIRE(fixture_runtime_empty());
  REQUIRE(fixture_clock_reads>0 && fixture_clock_reads<=128);
  REQUIRE(ku_test_event_set(&fixture_context->events[FIXTURE_A_TAKEN]));
  return fixture_healthy();
}
static int fixture_thread_b(void* raw) {
  fixture_context=(FixtureRound*)raw;
  fixture_role=2;
  REQUIRE(ku_test_event_wait(&fixture_context->events[FIXTURE_A_CLEANUP],FIXTURE_WAIT_MS));
  // A is blocked inside a live Ku cleanup frame. Neither A's deadline nor its
  // depth is allowed to appear in this second actual OS thread.
  REQUIRE(fixture_runtime_empty());
  __ku_sync_reset();
  int64_t result=FatalRoot();
  REQUIRE(!fixture_failed && fixture_enter_seen==1 && !fixture_cleanup_seen);
  REQUIRE(__ku_sync_return_signal.kind==KU_SYNC_EXIT_ARITHMETIC_FATAL);
  REQUIRE(__ku_sync_return_signal.arithmetic_status==KU_INT_DIV_ZERO);
  REQUIRE(!__ku_call_depth && !__ku_handler_unwind_depth);
  REQUIRE(!__ku_handler_timed_out && !__ku_handler_deadline && !__ku_handler_cleanup_deadline);
  REQUIRE(ku_test_event_set(&fixture_context->events[FIXTURE_B_PENDING]));
  REQUIRE(ku_test_event_wait(&fixture_context->events[FIXTURE_A_RETURNED],FIXTURE_WAIT_MS));
  // Both real return mailboxes are now populated with different kind/status.
  REQUIRE(__ku_sync_return_signal.kind==KU_SYNC_EXIT_ARITHMETIC_FATAL);
  REQUIRE(__ku_sync_return_signal.arithmetic_status==KU_INT_DIV_ZERO);
  REQUIRE(!__ku_call_depth && !__ku_handler_unwind_depth);
  REQUIRE(!__ku_handler_timed_out && !__ku_handler_deadline && !__ku_handler_cleanup_deadline);
  KuSyncExitSignal signal=__ku_sync_take();
  REQUIRE(signal.kind==KU_SYNC_EXIT_ARITHMETIC_FATAL && signal.arithmetic_status==KU_INT_DIV_ZERO);
  REQUIRE(!strcmp(__ku_sync_error_message(signal),"division by zero"));
  REQUIRE(result==0);
  REQUIRE(fixture_runtime_empty());
  REQUIRE(ku_test_event_set(&fixture_context->events[FIXTURE_B_TAKEN]));
  REQUIRE(ku_test_event_wait(&fixture_context->events[FIXTURE_A_TAKEN],FIXTURE_WAIT_MS));
  REQUIRE(fixture_runtime_empty());
  REQUIRE(__ku_handler_timeout_finish()==0);
  REQUIRE(fixture_clock_reads<=128);
  return fixture_healthy();
}
static int fixture_join(KuTestThread* thread) {
  if (!ku_test_thread_join(thread,FIXTURE_JOIN_MS)) {
    // Do not destroy shared events/storage while a thread might still access
    // them. A hard join failure terminates this bounded child process; the OS
    // reclaims its handles and the outer watchdog reaps it if it cannot exit.
    fputs("TLS arithmetic bounded join failed\n",stderr); abort();
  }
  return thread->outcome;
}
static int fixture_round(void) {
  FixtureRound round;
  unsigned initialized=0;
  for (; initialized<FIXTURE_EVENT_COUNT; ++initialized) {
    if (!ku_test_event_init(&round.events[initialized])) break;
  }
  if (initialized!=FIXTURE_EVENT_COUNT) {
    while (initialized) (void)ku_test_event_destroy(&round.events[--initialized]);
    fputs("TLS arithmetic event initialization failed\n",stderr);
    return 1;
  }
  KuTestThread a,b;
  int started_a=ku_test_thread_start(&a,fixture_thread_a,&round);
  int started_b=started_a && ku_test_thread_start(&b,fixture_thread_b,&round);
  int outcome=0;
  if (!started_a || !started_b) {
    outcome=1;
    fputs("TLS arithmetic thread initialization failed\n",stderr);
    (void)fixture_wake_all(&round);
  }
  if (started_a && fixture_join(&a)) outcome=1;
  if (started_b && fixture_join(&b)) outcome=1;
  // Both native threads are joined before any shared event is destroyed. Event
  // wait failures return nonzero through this same joined/closed failure path.
  while (initialized) {
    if (!ku_test_event_destroy(&round.events[--initialized])) outcome=1;
  }
  if (!fixture_runtime_empty()) outcome=1; /* the main OS thread is a third context */
  return outcome;
}
int main(void) {
  if (!fixture_runtime_empty()) return 1;
  for (unsigned round=0; round<4; ++round) {
    if (fixture_round()) return 1;
  }
  fputs("sync-tls-isolation-ok\n",stdout);
  return 0;
}
"#;

#[test]
fn native_sync_arithmetic_mailbox_timeout_and_depth_are_thread_local() {
    let ast = Parser::new(Lexer::new(SOURCE).lex().expect("TLS source lexes"))
        .parse_program()
        .expect("TLS source parses");
    Checker::new()
        .check(&ast)
        .unwrap_or_else(|error| panic!("{error}\n{SOURCE}"));
    let lowered = ir::lower_program(&ast).expect("TLS source lowers within budget");
    let generated = c::generate_c_source(&ir::optimize_program(&lowered)).expect("native C emits");
    for helper in ["add", "div"] {
        assert_eq!(
            generated
                .matches(&format!("static uint32_t ku_int_{helper}("))
                .count(),
            1
        );
        assert!(
            generated.matches(&format!("ku_int_{helper}(")).count() > 1,
            "never execute an old unchecked arithmetic artifact"
        );
    }
    assert!(generated.contains("KuSyncExitSignal"));
    let generated = replace_once(
        generated,
        "typedef struct KuString {",
        "static void fixture_observe_print(unsigned phase);\ntypedef struct KuString {",
    );
    let tls_anchor = "static KU_THREAD_LOCAL long __ku_call_depth = 0;";
    let generated = replace_once(
        generated,
        tls_anchor,
        &format!("{NATIVE_THREAD_LIFECYCLE_HARNESS}\n{FIXTURE_GLOBALS}\n{tls_anchor}"),
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
    let directory = TempDir::new("native-sync-tls-isolation");
    let path = directory.path().join("program.c");
    fs::write(&path, format!("{generated}\n{C_MAIN}")).unwrap();
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
    .expect("TLS child obeys the real process watchdog");
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
        "B-enter\nA-cleanup\n".repeat(4) + "sync-tls-isolation-ok\n"
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
