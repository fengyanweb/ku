//! Existing timeout cleanup: per-pending-return boundaries, not a frame flag.
//! Source/Checker/IR/generated C are real. Only the clock and observations are
//! hooked; no Task/raw API, replacement control flow, or fake successful result.
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
fn failure(): str! {
    fail { domain: "owned-" + "domain", code: "owned-" + "code", message: "owned-" + "message" }
}
fn escape_fail(stop: bool): str! {
    try {
        try { while (stop) {} return ok("saved-" + "body") }
        finally {
            println("fail-inner")
            fail { domain: "owned-" + "domain", code: "owned-" + "code", message: "owned-" + "message" }
        }
    } catch (e) {
        println("caught-fail") println(e.domain) println(e.code) println(e.message)
        return ok("recover-" + "fail")
    } finally { println("fail-outer") }
    return ok("BAD")
}
fn escape_question(stop: bool): str! {
    try {
        try { while (stop) {} return ok("saved-" + "body") }
        finally { println("question-inner") value = failure()? println("BAD-after-question") }
    } catch (e) {
        println("caught-question") println(e.domain) println(e.code) println(e.message)
        return ok("recover-" + "question")
    } finally { println("question-outer") }
    return ok("BAD")
}
fn escape_return(stop: bool): str! {
    try {
        try { while (stop) {} return ok("saved-" + "body") }
        finally { println("return-inner") return ok("replacement-" + "owned") }
    } catch (e) { println("BAD-return-catch") return ok("BAD") }
    finally { println("return-outer") }
    return ok("BAD")
}
fn direct_fail(stop: bool): str! {
    try { while (stop) {} return ok("saved-" + "body") }
    finally {
        println("direct-fail")
        fail { domain: "owned-" + "domain", code: "owned-" + "code", message: "owned-" + "message" }
    }
    return ok("BAD")
}
fn direct_question(stop: bool): str! {
    try { while (stop) {} return ok("saved-" + "body") }
    finally { println("direct-question") value = failure()? println("BAD-direct-question") }
    return ok("BAD")
}
fn error_return(stop: bool): str! {
    try { while (stop) {} return ok("saved-" + "body") }
    finally { println("error-return") return failure() }
    return ok("BAD")
}
fn bare_return(stop: bool): str {
    try { while (stop) {} return "saved-" + "body" }
    finally { println("bare-return") return "replacement-" + "bare" }
    return "BAD"
}
fn two_false_floors(stop: bool): str! {
    try { while (stop) {} }
    finally {
        for i in 2 {
            try {
                try { return ok("discard-" + "first") }
                finally {
                    try { return ok("discard-" + "second") }
                    finally {
                        fail { domain: "owned-" + "domain", code: "owned-" + "code", message: "owned-" + "message" }
                    }
                }
            } catch (e) { println("local-two") println(e.message) }
        }
        println("after-two")
    }
    return ok("normal-" + "done")
}
fn log_void() { println("void-call") }
fn void_return(stop: bool) {
    try { while (stop) {} return }
    finally { return log_void() }
}
fn exhaust_cleanup(): str! {
    try {
        try { while (true) {} }
        finally { println("exhaust-inner") while (true) {} println("BAD-exhaust-inner") }
    } catch (e) { println("BAD-exhaust-catch") return ok("BAD") }
    finally { println("exhaust-outer") }
    return ok("BAD")
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

const OBSERVER_GLOBALS: &str = r#"
static unsigned long long fixture_now=100;
static unsigned fixture_exhaust,fixture_clock_reads,fixture_expired_polls;
static char fixture_trace[2048];
static size_t fixture_trace_len;
"#;

// Observes real string output. It neither changes clock/control state nor
// suppresses printing; the Rust assertion also checks actual stdout in order.
const OBSERVE_PRINT: &str = r#"
  if (stream == stdout) {
    if (value.len > sizeof(fixture_trace)-fixture_trace_len-2) {
      fputs("cleanup trace overflow\n",stderr); abort();
    }
    if (value.len) memcpy(fixture_trace+fixture_trace_len,value.ptr,value.len);
    fixture_trace_len+=value.len;
    fixture_trace[fixture_trace_len++]='|';
    fixture_trace[fixture_trace_len]=0;
  }
"#;

// Fixed virtual time creates the initial timeout, then either stays within its
// grace or reaches precisely the original D on the next poll. The finite clock
// call cap is a failure bound, never an expected-success timeout. The separate
// real process watchdog remains the final bound if no further clock call occurs.
const CLOCK: &str = r#"
static unsigned long long __ku_handler_now_ms(void) {
  if (++fixture_clock_reads > 1024) {
    fputs("cleanup clock progress bound exceeded\n",stderr); abort();
  }
  if (fixture_exhaust && __ku_handler_cleanup_deadline) {
    fixture_expired_polls++;
    return __ku_handler_cleanup_deadline;
  }
  return fixture_now;
}
"#;

const C_MAIN: &str = r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr,"cleanup matrix line %d: %s\n",__LINE__,#c); abort(); } } while (0)
static int fixture_string(KuString value,const char* expected) {
  size_t length=strlen(expected);
  return value.len==length && (!length || (value.ptr && !memcmp(value.ptr,expected,length)));
}
static int fixture_empty_string(KuString value) {
  return !value.ptr && !value.len && !value.capacity && !value.storage;
}
static void fixture_error(KuError value,int empty) {
  if (empty) {
    CHECK(fixture_empty_string(value.domain));
    CHECK(fixture_empty_string(value.code));
    CHECK(fixture_empty_string(value.message));
  } else {
    CHECK(fixture_string(value.domain,"owned-domain"));
    CHECK(fixture_string(value.code,"owned-code"));
    CHECK(fixture_string(value.message,"owned-message"));
  }
}
static size_t fixture_begin(int timed,int exhaust) {
  CHECK(!__ku_handler_deadline && !__ku_handler_cleanup_deadline);
  CHECK(!__ku_handler_timed_out && !__ku_handler_unwind_depth && !__ku_call_depth);
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  fixture_trace_len=0; fixture_trace[0]=0;
  fixture_clock_reads=0; fixture_expired_polls=0;
  fixture_now=100; fixture_exhaust=(unsigned)exhaust;
  if (timed) { __ku_handler_timeout_begin(1); fixture_now=101; }
  return ku_perf_calls;
}
static void fixture_finish(int timed,int require_allocation,size_t before,const char* trace) {
  CHECK(!strcmp(fixture_trace,trace));
  CHECK(!__ku_call_depth && !__ku_handler_unwind_depth);
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  if (require_allocation) CHECK(ku_perf_calls>before);
  if (timed) {
    CHECK(__ku_handler_timed_out);
    CHECK(__ku_handler_deadline==101 && __ku_handler_cleanup_deadline==1101);
    if (fixture_exhaust) CHECK(fixture_expired_polls>0);
    CHECK(__ku_handler_timeout_finish()==1);
  } else {
    CHECK(!__ku_handler_timed_out && !__ku_handler_cleanup_deadline);
    CHECK(__ku_handler_timeout_finish()==0);
  }
  CHECK(!__ku_handler_deadline && !__ku_handler_cleanup_deadline);
  CHECK(!__ku_handler_timed_out && !__ku_handler_unwind_depth && !__ku_call_depth);
}
typedef KuResult_str (*FixtureResultCase)(bool);
static void fixture_result_case(FixtureResultCase function,int timed,const char* trace,const char* ok_value) {
  size_t before=fixture_begin(timed,0);
  KuResult_str result=function(timed!=0);
  if (timed) {
    CHECK(!result.ok && fixture_empty_string(result.value));
    fixture_error(result.error,1);
  } else if (ok_value) {
    CHECK(result.ok && fixture_string(result.value,ok_value));
    fixture_error(result.error,1);
  } else {
    CHECK(!result.ok && fixture_empty_string(result.value));
    fixture_error(result.error,0);
  }
  ku_result_drop_str(&result);
  fixture_finish(timed,1,before,trace);
}
int main(void) {
  for (unsigned round=0; round<16; ++round) {
    fixture_result_case(escape_fail,1,"fail-inner|fail-outer|",NULL);
    fixture_result_case(escape_fail,0,"fail-inner|caught-fail|owned-domain|owned-code|owned-message|fail-outer|","recover-fail");
    fixture_result_case(escape_question,1,"question-inner|question-outer|",NULL);
    fixture_result_case(escape_question,0,"question-inner|caught-question|owned-domain|owned-code|owned-message|question-outer|","recover-question");
    fixture_result_case(escape_return,1,"return-inner|return-outer|",NULL);
    fixture_result_case(escape_return,0,"return-inner|return-outer|","replacement-owned");
    fixture_result_case(direct_fail,1,"direct-fail|",NULL);
    fixture_result_case(direct_fail,0,"direct-fail|",NULL);
    fixture_result_case(direct_question,1,"direct-question|",NULL);
    fixture_result_case(direct_question,0,"direct-question|",NULL);
    fixture_result_case(error_return,1,"error-return|",NULL);
    fixture_result_case(error_return,0,"error-return|",NULL);
    fixture_result_case(two_false_floors,1,"local-two|owned-message|local-two|owned-message|after-two|",NULL);
    fixture_result_case(two_false_floors,0,"local-two|owned-message|local-two|owned-message|after-two|","normal-done");
    for (int timed=1; timed>=0; --timed) {
      size_t before=fixture_begin(timed,0);
      KuString result=bare_return(timed!=0);
      CHECK(timed ? fixture_empty_string(result) : fixture_string(result,"replacement-bare"));
      ku_string_drop(&result);
      fixture_finish(timed,1,before,"bare-return|");
    }
    for (int mode=0; mode<3; ++mode) {
      int timed=mode!=0;
      size_t before=fixture_begin(timed,mode==2);
      // Normal source Return and selected-timeout paths both execute log_void
      // once before the boundary guard; mode 2 additionally expires at its
      // caller's post-call safepoint, after the observed call side effect.
      void_return(timed!=0);
      fixture_finish(timed,0,before,"void-call|");
    }
    size_t before=fixture_begin(1,1);
    KuResult_str result=exhaust_cleanup();
    CHECK(!result.ok && fixture_empty_string(result.value));
    fixture_error(result.error,1);
    ku_result_drop_str(&result);
    fixture_finish(1,0,before,"exhaust-inner|exhaust-outer|");
  }
  fputs("cleanup-matrix-ok\n",stdout);
  return 0;
}
"#;

const ROUND_EXPECTED: &str = concat!(
    "fail-inner\nfail-outer\n",
    "fail-inner\ncaught-fail\nowned-domain\nowned-code\nowned-message\nfail-outer\n",
    "question-inner\nquestion-outer\n",
    "question-inner\ncaught-question\nowned-domain\nowned-code\nowned-message\nquestion-outer\n",
    "return-inner\nreturn-outer\nreturn-inner\nreturn-outer\n",
    "direct-fail\ndirect-fail\ndirect-question\ndirect-question\n",
    "error-return\nerror-return\n",
    "local-two\nowned-message\nlocal-two\nowned-message\nafter-two\n",
    "local-two\nowned-message\nlocal-two\nowned-message\nafter-two\n",
    "bare-return\nbare-return\nvoid-call\nvoid-call\nvoid-call\nexhaust-inner\nexhaust-outer\n",
);

#[test]
fn native_cleanup_escape_matrix_preserves_local_recovery_and_owned_payloads() {
    let ast = Parser::new(Lexer::new(SOURCE).lex().expect("source lexes"))
        .parse_program()
        .expect("source parses");
    Checker::new()
        .check(&ast)
        .unwrap_or_else(|error| panic!("{error}\n{SOURCE}"));
    let lowered = ir::lower_program(&ast).expect("source lowers within existing budget");
    let generated = c::generate_c_source(&ir::optimize_program(&lowered)).expect("native C emits");
    let generated = replace_once(
        generated,
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
    let directory = TempDir::new("native-cleanup-escape");
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
    .expect("cleanup execution obeys the real process watchdog");
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
        ROUND_EXPECTED.repeat(16) + "cleanup-matrix-ok\n"
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
