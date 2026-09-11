//! First timeout selected while a normal or UserReturn finally is in progress.
//! Only clock reads and print observations are hooked. The compiler's real
//! Safepoint chooses the timeout route; no runtime control field is assigned.
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
fn make_saved(): str {
    value = "saved-" + "owned"
    println(value)
    return value
}
fn poll_point() { println("poll-point") }
fn outer_probe() { println("outer-probe") }

fn ordinary_finally(): str! {
    saved = make_saved()
    try {
        try { println("ordinary-body") }
        finally {
            println("ordinary-enter")
            poll_point()
            println("ordinary-after")
        }
    } catch (e) { println("BAD-catch") return ok("BAD") }
    finally {
        println("outer")
        outer_probe()
        println("outer-after")
    }
    return ok(saved)
}
fn return_finally(): str! {
    try {
        try { return ok(make_saved()) }
        finally {
            println("return-enter")
            poll_point()
            println("return-after")
        }
    } catch (e) { println("BAD-catch") return ok("BAD") }
    finally {
        println("outer")
        outer_probe()
        println("outer-after")
    }
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

// Declared before KuString and its helpers. The observer is defined after all
// generated C so it can inspect the actual later-declared timeout TLS fields.
const OBSERVER_GLOBALS: &str = r#"
static unsigned fixture_timed,fixture_enter_seen,fixture_poll_point_seen;
static unsigned fixture_saved_seen,fixture_outer_seen,fixture_outer_probe_seen;
static unsigned fixture_outer_after_seen,fixture_clock_reads,fixture_selection_reads;
static unsigned fixture_grace_reads;
static uintptr_t fixture_saved_pointer;
static char fixture_trace[512];
static size_t fixture_trace_len;
static void fixture_observe_phase(unsigned phase);
"#;

// Pure observer: it records real output without suppressing it, setting the
// clock, changing a timeout field, or intercepting a drop/return/branch helper.
const OBSERVE_PRINT: &str = r#"
  if (stream == stdout) {
    if (fixture_trace_len > sizeof(fixture_trace)-2 ||
        value.len > sizeof(fixture_trace)-fixture_trace_len-2) {
      fputs("first cleanup trace overflow\n",stderr); abort();
    }
    if (value.len) memcpy(fixture_trace+fixture_trace_len,value.ptr,value.len);
    fixture_trace_len+=value.len;
    fixture_trace[fixture_trace_len++]='|';
    fixture_trace[fixture_trace_len]=0;
    if (value.len==11 && !memcmp(value.ptr,"saved-owned",11)) {
      if (value.storage!=KU_STRING_OWNED || !value.ptr) {
        fputs("saved return must be a real owned string\n",stderr); abort();
      }
      fixture_saved_pointer=(uintptr_t)value.ptr;
      fixture_observe_phase(1);
    } else if ((value.len==14 && !memcmp(value.ptr,"ordinary-enter",14)) ||
               (value.len==12 && !memcmp(value.ptr,"return-enter",12))) {
      fixture_observe_phase(2);
    } else if (value.len==10 && !memcmp(value.ptr,"poll-point",10)) {
      fixture_observe_phase(3);
    } else if (value.len==5 && !memcmp(value.ptr,"outer",5)) {
      fixture_observe_phase(4);
    } else if (value.len==11 && !memcmp(value.ptr,"outer-probe",11)) {
      fixture_observe_phase(5);
    } else if (value.len==11 && !memcmp(value.ptr,"outer-after",11)) {
      fixture_observe_phase(6);
    }
  }
"#;

// All pre-finally polls see 100. Even the poll-point print observes no selected
// timeout. Its direct caller's real post-call Safepoint first sees 101, selects
// timeout, and creates D=1101. Later grace polls see 600, not a renewed budget.
// The finite call cap is a failure bound, not the intended execution outcome.
const CLOCK: &str = r#"
static unsigned long long __ku_handler_now_ms(void) {
  if (++fixture_clock_reads > 128) {
    fputs("first cleanup clock progress bound exceeded\n",stderr); abort();
  }
  if (__ku_handler_cleanup_deadline) {
    if (__ku_handler_cleanup_deadline!=1101) {
      fputs("first cleanup deadline was renewed\n",stderr); abort();
    }
    fixture_grace_reads++;
    return 600;
  }
  if (fixture_timed && fixture_poll_point_seen) {
    if (!__ku_handler_timed_out) fixture_selection_reads++;
    return 101;
  }
  return 100;
}
"#;

const C_MAIN: &str = r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr,"first cleanup line %d: %s\n",__LINE__,#c); abort(); } } while (0)
static int fixture_string(KuString value,const char* expected) {
  size_t length=strlen(expected);
  return value.len==length && (!length || (value.ptr && !memcmp(value.ptr,expected,length)));
}
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
  if (phase<=3) {
    CHECK(!__ku_handler_timed_out && !__ku_handler_cleanup_deadline);
    CHECK(!__ku_handler_unwind_depth && __ku_handler_deadline==101);
    CHECK(ku_perf_live_allocations==1 && ku_perf_live_bytes>0);
  }
  switch (phase) {
    case 1:
      CHECK(!fixture_saved_seen && !fixture_enter_seen);
      fixture_saved_seen++;
      break;
    case 2:
      CHECK(fixture_saved_seen==1 && !fixture_enter_seen);
      fixture_enter_seen++;
      break;
    case 3:
      CHECK(fixture_enter_seen==1 && !fixture_poll_point_seen);
      fixture_poll_point_seen++;
      break;
    case 4:
      CHECK(fixture_poll_point_seen==1 && !fixture_outer_seen);
      fixture_outer_seen++;
      break;
    case 5:
      CHECK(fixture_outer_seen==1 && !fixture_outer_probe_seen);
      fixture_outer_probe_seen++;
      break;
    case 6:
      CHECK(fixture_outer_probe_seen==1 && !fixture_outer_after_seen);
      fixture_outer_after_seen++;
      break;
    default: CHECK(0);
  }
  if (phase>=4) {
    CHECK(__ku_handler_deadline==101);
    if (fixture_timed) {
      CHECK(__ku_handler_timed_out && __ku_handler_unwind_depth>0);
      CHECK(__ku_handler_cleanup_deadline==1101);
      // Earlier release is legal. The function must have no live owner on its
      // timed return; do not pin a particular structural drop instruction here.
      CHECK(ku_perf_live_allocations<=1);
    } else {
      CHECK(!__ku_handler_timed_out && !__ku_handler_cleanup_deadline);
      CHECK(!__ku_handler_unwind_depth);
      CHECK(ku_perf_live_allocations==1 && ku_perf_live_bytes>0);
    }
  }
}
static size_t fixture_begin(int timed) {
  CHECK(!__ku_handler_deadline && !__ku_handler_cleanup_deadline);
  CHECK(!__ku_handler_timed_out && !__ku_handler_unwind_depth && !__ku_call_depth);
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  fixture_timed=(unsigned)timed;
  fixture_enter_seen=fixture_poll_point_seen=fixture_saved_seen=0;
  fixture_outer_seen=fixture_outer_probe_seen=fixture_outer_after_seen=0;
  fixture_clock_reads=fixture_selection_reads=fixture_grace_reads=0;
  fixture_saved_pointer=0;
  fixture_trace_len=0; fixture_trace[0]=0;
  __ku_handler_timeout_begin(1);
  CHECK(__ku_handler_deadline==101 && !__ku_handler_timed_out);
  return ku_perf_calls;
}
typedef KuResult_str (*FixtureCase)(void);
static void fixture_case(FixtureCase function,int timed,const char* expected) {
  size_t before=fixture_begin(timed);
  KuResult_str result=function();
  CHECK(!strcmp(fixture_trace,expected));
  CHECK(fixture_saved_seen==1 && fixture_enter_seen==1 && fixture_poll_point_seen==1);
  CHECK(fixture_outer_seen==1 && fixture_outer_probe_seen==1 && fixture_outer_after_seen==1);
  CHECK(!__ku_call_depth && !__ku_handler_unwind_depth);
  // Exactly one source concat: normal returns move that allocation; timeout
  // discards it before leaving the function, not in the C test's result drop.
  CHECK(ku_perf_calls-before==1);
  fixture_empty_error(result.error);
  if (timed) {
    CHECK(!result.ok && fixture_empty_string(result.value));
    CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes);
    CHECK(fixture_selection_reads==1 && fixture_grace_reads>0);
    CHECK(__ku_handler_timed_out && __ku_handler_cleanup_deadline==1101);
  } else {
    CHECK(result.ok && fixture_string(result.value,"saved-owned"));
    CHECK(result.value.storage==KU_STRING_OWNED);
    CHECK((uintptr_t)result.value.ptr==fixture_saved_pointer);
    CHECK(ku_perf_live_allocations==1 && ku_perf_live_bytes>0);
    CHECK(!fixture_selection_reads && !fixture_grace_reads);
    CHECK(!__ku_handler_timed_out && !__ku_handler_cleanup_deadline);
  }
  CHECK(__ku_handler_deadline==101);
  ku_result_drop_str(&result);
  CHECK(!result.ok && fixture_empty_string(result.value));
  fixture_empty_error(result.error);
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  CHECK(__ku_handler_timeout_finish()==timed);
  CHECK(!__ku_handler_deadline && !__ku_handler_cleanup_deadline);
  CHECK(!__ku_handler_timed_out && !__ku_handler_unwind_depth && !__ku_call_depth);
}
int main(void) {
  for (unsigned round=0; round<16; ++round) {
    fixture_case(ordinary_finally,0,"saved-owned|ordinary-body|ordinary-enter|poll-point|ordinary-after|outer|outer-probe|outer-after|");
    fixture_case(ordinary_finally,1,"saved-owned|ordinary-body|ordinary-enter|poll-point|outer|outer-probe|outer-after|");
    fixture_case(return_finally,0,"saved-owned|return-enter|poll-point|return-after|outer|outer-probe|outer-after|");
    fixture_case(return_finally,1,"saved-owned|return-enter|poll-point|outer|outer-probe|outer-after|");
  }
  fputs("first-cleanup-timeout-ok\n",stdout);
  return 0;
}
"#;

const ROUND_EXPECTED: &str = concat!(
    "saved-owned\nordinary-body\nordinary-enter\npoll-point\nordinary-after\nouter\nouter-probe\nouter-after\n",
    "saved-owned\nordinary-body\nordinary-enter\npoll-point\nouter\nouter-probe\nouter-after\n",
    "saved-owned\nreturn-enter\npoll-point\nreturn-after\nouter\nouter-probe\nouter-after\n",
    "saved-owned\nreturn-enter\npoll-point\nouter\nouter-probe\nouter-after\n",
);

#[test]
fn native_first_cleanup_timeout_discards_saved_owner_and_preserves_original_deadline() {
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
    let directory = TempDir::new("native-first-cleanup-timeout");
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
    .expect("first-cleanup-timeout execution obeys the real process watchdog");
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
        ROUND_EXPECTED.repeat(16) + "first-cleanup-timeout-ok\n"
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
