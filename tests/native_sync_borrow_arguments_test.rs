//! Real synchronous arithmetic failure during borrowed-argument evaluation.
//! Actual allocation identities, not just totals, prove the two fresh roots
//! drop in reverse order before cleanup reads the caller's original owner.
#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use ku::{backend::c, checker::Checker, ir, lexer::Lexer, parser::Parser};
use native_allocation_harness::ALLOCATION_HOOK;
use native_harness::{compile_harness, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};
use std::{fs, process::Command};

// Ordinary calls use the same source: timed=false,d=3 succeeds;
// timed=false,d=0 fails after root-finally has already started, so it must not
// enter Evaluate's own finally or the root's nested finally. Selected timeout
// alone permits those remaining cleanup attempts under the existing deadline.
const SOURCE: &str = r#"
fn Fresh(label: str): str {
    value = label + "-temp"
    println(value)
    return value
}
fn Divide(n: int, d: int): int { return n / d }
fn Later(): int { println("later") return 3 }
fn Sink(&seed: str, &first: str, &second: str, n: int, last: int): int {
    println("sink")
    return n
}
fn Evaluate(&seed: str, d: int): int {
    try {
        return Sink(seed, Fresh("first"), Fresh("second"), Divide(12, d), Later())
    } finally {
        println("inner")
        println(seed)
    }
    return -1
}
fn RootCase(timed: bool, d: int): int! {
    local = "local-" + "owner"
    println(local)
    try { if (timed) { while (true) {} } }
    finally {
        println("root-finally")
        try {
            value = Evaluate(local, d)
            println("after-evaluate")
        } finally {
            println("outer")
            println(local)
        }
        println("after-outer")
    }
    return ok(1)
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

// Precede the allocation harness: free() observation is declared before the
// ledger's real free wrapper. Bodies appear after all generated runtime types.
const OBSERVER_GLOBALS: &str = r#"
static unsigned fixture_mode,fixture_local_reads,fixture_root_finally;
static unsigned fixture_first_seen,fixture_second_seen,fixture_later,fixture_sink;
static unsigned fixture_inner,fixture_outer,fixture_after_evaluate,fixture_after_outer;
static unsigned fixture_clock_reads,fixture_selection_reads,fixture_grace_reads;
static unsigned fixture_step,fixture_inner_step,fixture_free_count;
static unsigned fixture_free_ids[3],fixture_free_steps[3],fixture_freed[3];
static uintptr_t fixture_pointers[3]; /* local, first, second: simultaneously live */
static char fixture_trace[512];
static size_t fixture_trace_len;
static void fixture_observe_print(unsigned phase,uintptr_t pointer,uint8_t storage);
static void fixture_observe_free(void* value);
"#;

// Only records the real print and its pointer identity. The stdout assertion
// independently checks all visible statements and their source order.
const OBSERVE_PRINT: &str = r#"
  if (stream == stdout) {
    if (fixture_trace_len > sizeof(fixture_trace)-2 ||
        value.len > sizeof(fixture_trace)-fixture_trace_len-2) {
      fputs("borrow arithmetic trace overflow\n",stderr); abort();
    }
    if (value.len) memcpy(fixture_trace+fixture_trace_len,value.ptr,value.len);
    fixture_trace_len+=value.len;
    fixture_trace[fixture_trace_len++]='|';
    fixture_trace[fixture_trace_len]=0;
    unsigned phase=0;
    if (value.len==11 && !memcmp(value.ptr,"local-owner",11)) phase=1;
    else if (value.len==12 && !memcmp(value.ptr,"root-finally",12)) phase=2;
    else if (value.len==10 && !memcmp(value.ptr,"first-temp",10)) phase=3;
    else if (value.len==11 && !memcmp(value.ptr,"second-temp",11)) phase=4;
    else if (value.len==5 && !memcmp(value.ptr,"later",5)) phase=5;
    else if (value.len==4 && !memcmp(value.ptr,"sink",4)) phase=6;
    else if (value.len==5 && !memcmp(value.ptr,"inner",5)) phase=7;
    else if (value.len==14 && !memcmp(value.ptr,"after-evaluate",14)) phase=8;
    else if (value.len==5 && !memcmp(value.ptr,"outer",5)) phase=9;
    else if (value.len==11 && !memcmp(value.ptr,"after-outer",11)) phase=10;
    fixture_observe_print(phase,(uintptr_t)value.ptr,value.storage);
  }
"#;

// Clock is the only injected control input. It selects a timeout at the real
// loop poll after the first local-owner print, then stays inside original D.
const CLOCK: &str = r#"
static unsigned long long __ku_handler_now_ms(void) {
  if (++fixture_clock_reads>256) {
    fputs("borrow arithmetic progress bound exceeded\n",stderr); abort();
  }
  if (__ku_handler_cleanup_deadline) {
    if (__ku_handler_cleanup_deadline!=1101) {
      fputs("borrow arithmetic cleanup deadline renewed\n",stderr); abort();
    }
    fixture_grace_reads++;
    return 600;
  }
  if (fixture_mode==1 && fixture_local_reads) {
    if (!__ku_handler_timed_out) fixture_selection_reads++;
    return 101;
  }
  return 100;
}
"#;

const C_MAIN: &str = r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr,"borrow arithmetic line %d: %s\n",__LINE__,#c); abort(); } } while (0)
static int fixture_empty_string(KuString value) {
  return !value.ptr && !value.len && !value.capacity && !value.storage;
}
static void fixture_empty_error(KuError error) {
  CHECK(fixture_empty_string(error.domain));
  CHECK(fixture_empty_string(error.code));
  CHECK(fixture_empty_string(error.message));
}
static void fixture_observe_free(void* value) {
  CHECK(value && fixture_free_count<3);
  uintptr_t pointer=(uintptr_t)value;
  unsigned id=0;
  for (; id<3; ++id) if (fixture_pointers[id]==pointer) break;
  CHECK(id<3 && fixture_pointers[id] && !fixture_freed[id]);
  if (id==0) {
    CHECK(fixture_free_count==2 && fixture_freed[1] && fixture_freed[2]);
    if (fixture_mode==2) CHECK(!fixture_inner && !fixture_outer && fixture_local_reads==1);
    else CHECK(fixture_inner==1 && fixture_outer==1 && fixture_local_reads==3);
  } else {
    CHECK(!fixture_inner && !fixture_outer && !fixture_freed[0]);
    // On abandonment the generated explicit cleanup and ordinary frame
    // epilogue both must release second then first, never the borrowed local.
    if (fixture_mode!=0) CHECK(id==2-fixture_free_count);
  }
  fixture_free_ids[fixture_free_count]=id;
  fixture_free_steps[fixture_free_count]=++fixture_step;
  fixture_free_count++;
  fixture_freed[id]=1;
  // Do not free, alter a pointer/header, or touch the runtime mailbox here.
  // The unchanged ledger wrapper performs the real release after this returns.
}
static void fixture_observe_print(unsigned phase,uintptr_t pointer,uint8_t storage) {
  CHECK(phase && !ku_perf_overflow);
  ++fixture_step;
  CHECK(__ku_sync_return_signal.kind==KU_SYNC_EXIT_NONE);
  if (phase==1 && !fixture_local_reads) {
    CHECK(storage==KU_STRING_OWNED && pointer);
    CHECK(ku_perf_live_allocations==1 && !fixture_free_count);
    fixture_pointers[0]=pointer;
    CHECK(!__ku_handler_timed_out && !__ku_handler_cleanup_deadline);
  } else if (fixture_mode==1) {
    CHECK(__ku_handler_timed_out && __ku_handler_unwind_depth>0);
    CHECK(__ku_handler_deadline==101 && __ku_handler_cleanup_deadline==1101);
  } else {
    CHECK(!__ku_handler_timed_out && !__ku_handler_cleanup_deadline);
    CHECK(!__ku_handler_deadline && !__ku_handler_unwind_depth);
  }
  switch (phase) {
    case 1:
      CHECK(pointer==fixture_pointers[0] && storage==KU_STRING_OWNED && !fixture_freed[0]);
      CHECK(ku_perf_live_allocations==1 && ku_perf_live_bytes>0);
      if (fixture_local_reads==1) CHECK(fixture_inner==1 && !fixture_outer);
      if (fixture_local_reads==2) CHECK(fixture_inner==1 && fixture_outer==1);
      CHECK(fixture_local_reads<3);
      fixture_local_reads++;
      break;
    case 2:
      CHECK(fixture_local_reads==1 && !fixture_root_finally);
      fixture_root_finally++;
      break;
    case 3:
      CHECK(fixture_root_finally==1 && !fixture_first_seen && !fixture_second_seen);
      CHECK(storage==KU_STRING_OWNED && pointer && pointer!=fixture_pointers[0]);
      CHECK(ku_perf_live_allocations==2 && !fixture_free_count);
      fixture_pointers[1]=pointer;
      fixture_first_seen++;
      break;
    case 4:
      CHECK(fixture_first_seen==1 && !fixture_second_seen);
      CHECK(storage==KU_STRING_OWNED && pointer && pointer!=fixture_pointers[0] && pointer!=fixture_pointers[1]);
      CHECK(ku_perf_live_allocations==3 && !fixture_free_count);
      fixture_pointers[2]=pointer;
      fixture_second_seen++;
      break;
    case 5:
      CHECK(fixture_mode==0 && fixture_second_seen==1 && !fixture_later && !fixture_sink);
      fixture_later++;
      break;
    case 6:
      CHECK(fixture_mode==0 && fixture_later==1 && !fixture_sink);
      fixture_sink++;
      break;
    case 7:
      CHECK(fixture_mode!=2 && !fixture_inner && !fixture_outer);
      CHECK(fixture_free_count==2 && fixture_freed[1] && fixture_freed[2] && !fixture_freed[0]);
      CHECK(ku_perf_live_allocations==1);
      fixture_inner_step=fixture_step;
      if (fixture_mode==1) {
        CHECK(fixture_free_ids[0]==2 && fixture_free_ids[1]==1);
        CHECK(fixture_free_steps[0]<fixture_free_steps[1] && fixture_free_steps[1]<fixture_inner_step);
      }
      fixture_inner++;
      break;
    case 8:
      CHECK(fixture_mode==0 && fixture_inner==1 && fixture_local_reads==2 && !fixture_after_evaluate);
      fixture_after_evaluate++;
      break;
    case 9:
      CHECK(fixture_mode!=2 && fixture_inner==1 && fixture_local_reads==2 && !fixture_outer);
      CHECK(fixture_free_count==2 && !fixture_freed[0] && ku_perf_live_allocations==1);
      fixture_outer++;
      break;
    case 10:
      CHECK(fixture_mode==0 && fixture_outer==1 && fixture_local_reads==3 && !fixture_after_outer);
      fixture_after_outer++;
      break;
    default: CHECK(0);
  }
}
static void fixture_case(unsigned mode,const char* expected) {
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  CHECK(!__ku_handler_timed_out && !__ku_handler_deadline && !__ku_handler_cleanup_deadline);
  CHECK(!__ku_call_depth && !__ku_handler_unwind_depth);
  CHECK(__ku_sync_return_signal.kind==KU_SYNC_EXIT_NONE);
  __ku_sync_reset();
  fixture_mode=mode;
  fixture_local_reads=fixture_root_finally=fixture_first_seen=fixture_second_seen=0;
  fixture_later=fixture_sink=fixture_inner=fixture_outer=0;
  fixture_after_evaluate=fixture_after_outer=0;
  fixture_clock_reads=fixture_selection_reads=fixture_grace_reads=0;
  fixture_step=fixture_inner_step=fixture_free_count=0;
  memset(fixture_free_ids,0,sizeof(fixture_free_ids));
  memset(fixture_free_steps,0,sizeof(fixture_free_steps));
  memset(fixture_freed,0,sizeof(fixture_freed));
  memset(fixture_pointers,0,sizeof(fixture_pointers));
  fixture_trace_len=0; fixture_trace[0]=0;
  size_t before=ku_perf_calls;
  if (mode==1) __ku_handler_timeout_begin(1);
  KuResult_int result=RootCase(mode==1,mode==0 ? 3 : 0);
  // Root consumption precedes inspecting the typed transport Result.
  KuSyncExitSignal signal=__ku_sync_take();
  CHECK(__ku_sync_return_signal.kind==KU_SYNC_EXIT_NONE);
  CHECK(!strcmp(fixture_trace,expected));
  CHECK(fixture_root_finally==1 && fixture_first_seen==1 && fixture_second_seen==1);
  CHECK(fixture_free_count==3 && fixture_free_ids[2]==0);
  CHECK(fixture_freed[0] && fixture_freed[1] && fixture_freed[2]);
  CHECK(ku_perf_calls-before==3);
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  CHECK(!__ku_call_depth && !__ku_handler_unwind_depth);
  fixture_empty_error(result.error);
  if (mode==0) {
    CHECK(signal.kind==KU_SYNC_EXIT_NONE && result.ok && result.value==1);
    CHECK(fixture_later==1 && fixture_sink==1 && fixture_after_evaluate==1 && fixture_after_outer==1);
    CHECK(fixture_local_reads==3 && fixture_inner==1 && fixture_outer==1);
  } else {
    CHECK(signal.arithmetic_status==KU_INT_DIV_ZERO);
    CHECK(signal.kind==(mode==1 ? KU_SYNC_EXIT_CLEANUP_ABORT : KU_SYNC_EXIT_ARITHMETIC_FATAL));
    CHECK(!strcmp(__ku_sync_error_message(signal),"division by zero"));
    CHECK(!result.ok && result.value==0);
    CHECK(!fixture_later && !fixture_sink && !fixture_after_evaluate && !fixture_after_outer);
    if (mode==1) CHECK(fixture_local_reads==3 && fixture_inner==1 && fixture_outer==1);
    else CHECK(fixture_local_reads==1 && !fixture_inner && !fixture_outer);
  }
  if (mode==1) {
    CHECK(fixture_selection_reads==1 && fixture_grace_reads>0);
    CHECK(__ku_handler_timed_out && __ku_handler_deadline==101 && __ku_handler_cleanup_deadline==1101);
  } else CHECK(!__ku_handler_timed_out && !__ku_handler_deadline && !__ku_handler_cleanup_deadline);
  ku_result_drop_int(&result);
  CHECK(fixture_free_count==3 && !ku_perf_live_allocations && !ku_perf_live_bytes);
  CHECK(__ku_handler_timeout_finish()==(mode==1));
  CHECK(__ku_sync_take().kind==KU_SYNC_EXIT_NONE);
  CHECK(!__ku_handler_timed_out && !__ku_handler_deadline && !__ku_handler_cleanup_deadline);
}
int main(void) {
  for (unsigned round=0; round<8; ++round) {
    fixture_case(0,"local-owner|root-finally|first-temp|second-temp|later|sink|inner|local-owner|after-evaluate|outer|local-owner|after-outer|");
    fixture_case(1,"local-owner|root-finally|first-temp|second-temp|inner|local-owner|outer|local-owner|");
    fixture_case(2,"local-owner|root-finally|first-temp|second-temp|");
  }
  fputs("sync-borrow-arguments-ok\n",stdout);
  return 0;
}
"#;

#[test]
fn native_sync_failed_later_argument_releases_fresh_borrows_without_consuming_source() {
    let ast = Parser::new(Lexer::new(SOURCE).lex().expect("borrow source lexes"))
        .parse_program()
        .expect("borrow source parses");
    Checker::new()
        .check(&ast)
        .unwrap_or_else(|error| panic!("{error}\n{SOURCE}"));
    let lowered = ir::lower_program(&ast).expect("borrow source lowers within budget");
    let generated = c::generate_c_source(&ir::optimize_program(&lowered)).expect("native C emits");
    assert_eq!(generated.matches("static uint32_t ku_int_div(").count(), 1);
    assert!(
        generated.matches("ku_int_div(").count() > 1,
        "do not compile/run the old raw-C division path"
    );
    assert!(generated.contains("KuSyncExitSignal"));
    assert!(generated.contains("int64_t Evaluate(const KuString* seed, int64_t d)"));
    let generated = replace_once(
        generated,
        "typedef struct KuString {",
        &format!("{OBSERVER_GLOBALS}\n{ALLOCATION_HOOK}\ntypedef struct KuString {{"),
    );
    let generated = replace_once(
        generated,
        "static void ku_perf_free(void* value) {",
        "static void ku_perf_free(void* value) {\n  if (value) fixture_observe_free(value);",
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
    let directory = TempDir::new("native-sync-borrow-arguments");
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
    .expect("borrow arithmetic execution obeys the real process watchdog");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let round = concat!(
        "local-owner\nroot-finally\nfirst-temp\nsecond-temp\nlater\nsink\ninner\nlocal-owner\nafter-evaluate\nouter\nlocal-owner\nafter-outer\n",
        "local-owner\nroot-finally\nfirst-temp\nsecond-temp\ninner\nlocal-owner\nouter\nlocal-owner\n",
        "local-owner\nroot-finally\nfirst-temp\nsecond-temp\n",
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        round.repeat(8) + "sync-borrow-arguments-ok\n"
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
