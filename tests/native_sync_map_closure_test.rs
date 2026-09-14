//! Checked arithmetic through an owned captured mapper and partial str output.
//! The first returned string and the second callback's local are distinct real
//! allocations. Observers never substitute callback results or driver state.
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
fn ReadSources(&seed: str, &values: [int]) {
    println("sources")
    println(seed)
    println(values[0])
    println(values[2])
}
fn MapCase(timed: bool, fail_math: bool): null! {
    seed = "capture-" + "owner"
    values = [1, 2, 3]
    ReadSources(seed, values)
    println("armed")
    try { if (timed) { while (true) {} } }
    finally {
        try {
            mapped = values.map(fn(value) {
                if (value == 1) { println("one") }
                else if (value == 2) { println("two") }
                else { println("three") }
                piece = seed.clone()
                println(piece)
                if (fail_math) {
                    quotient = 12 / (value - 2)
                    println("math-ok")
                }
                return piece
            })
            println("map-complete")
            println(mapped.len())
        } finally {
            println("outer")
            ReadSources(seed, values)
        }
        println("after-outer")
    }
    return ok(null)
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
static unsigned fixture_mode,fixture_armed,fixture_source_reads,fixture_capture_reads;
static unsigned fixture_callbacks,fixture_math_ok,fixture_map_complete,fixture_outer,fixture_after_outer;
static unsigned fixture_clock_reads,fixture_selection_reads,fixture_grace_reads;
static unsigned fixture_step,fixture_piece_freed[3],fixture_piece_free_steps[3];
static unsigned fixture_seed_freed,fixture_input_freed;
static uintptr_t fixture_seed,fixture_input,fixture_pieces[3];
static char fixture_trace[512];
static size_t fixture_trace_len;
static void fixture_observe_free(void* value);
static void fixture_observe_sources(uintptr_t seed,uintptr_t input,size_t length);
static void fixture_observe_print(unsigned phase,uintptr_t pointer,uint8_t storage);
"#;

const OBSERVE_PRINT: &str = r#"
  if (stream==stdout) {
    if (fixture_trace_len>sizeof(fixture_trace)-2 || value.len>sizeof(fixture_trace)-fixture_trace_len-2) {
      fputs("map closure trace overflow\n",stderr); abort();
    }
    if (value.len) memcpy(fixture_trace+fixture_trace_len,value.ptr,value.len);
    fixture_trace_len+=value.len;
    fixture_trace[fixture_trace_len++]='|'; fixture_trace[fixture_trace_len]=0;
    unsigned phase=0;
    if (value.len==7 && !memcmp(value.ptr,"sources",7)) phase=1;
    else if (value.len==13 && !memcmp(value.ptr,"capture-owner",13)) phase=2;
    else if (value.len==5 && !memcmp(value.ptr,"armed",5)) phase=3;
    else if (value.len==3 && !memcmp(value.ptr,"one",3)) phase=4;
    else if (value.len==3 && !memcmp(value.ptr,"two",3)) phase=5;
    else if (value.len==5 && !memcmp(value.ptr,"three",5)) phase=6;
    else if (value.len==7 && !memcmp(value.ptr,"math-ok",7)) phase=7;
    else if (value.len==12 && !memcmp(value.ptr,"map-complete",12)) phase=8;
    else if (value.len==5 && !memcmp(value.ptr,"outer",5)) phase=9;
    else if (value.len==11 && !memcmp(value.ptr,"after-outer",11)) phase=10;
    fixture_observe_print(phase,(uintptr_t)value.ptr,value.storage);
  }
"#;

// Selected timeout is a real loop poll after armed. Keeping grace time at 600
// makes a stale mailbox, not expired time, responsible if healthy reads abort.
const CLOCK: &str = r#"
static unsigned long long __ku_handler_now_ms(void) {
  if (++fixture_clock_reads>384) {
    fputs("map closure progress bound exceeded\n",stderr); abort();
  }
  if (__ku_handler_cleanup_deadline) {
    if (__ku_handler_cleanup_deadline!=1101) {
      fputs("map closure deadline renewed\n",stderr); abort();
    }
    fixture_grace_reads++;
    return 600;
  }
  if (fixture_mode==1 && fixture_armed) {
    if (!__ku_handler_timed_out) fixture_selection_reads++;
    return 101;
  }
  return 100;
}
"#;

const C_MAIN: &str = r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr,"map closure line %d: %s\n",__LINE__,#c); abort(); } } while (0)
static int fixture_empty_string(KuString value) {
  return !value.ptr && !value.len && !value.capacity && !value.storage;
}
static void fixture_empty_error(KuError error) {
  CHECK(fixture_empty_string(error.domain));
  CHECK(fixture_empty_string(error.code));
  CHECK(fixture_empty_string(error.message));
}
static void fixture_observe_free(void* value) {
  uintptr_t pointer=(uintptr_t)value;
  CHECK(value);
  unsigned step=++fixture_step;
  if (pointer==fixture_seed && fixture_seed) {
    CHECK(!fixture_seed_freed);
    CHECK(fixture_source_reads==(fixture_mode==2 ? 1u : 2u));
    fixture_seed_freed=1;
  } else if (pointer==fixture_input && fixture_input) {
    CHECK(!fixture_input_freed);
    CHECK(fixture_source_reads==(fixture_mode==2 ? 1u : 2u));
    fixture_input_freed=1;
  } else {
    for (unsigned index=0; index<3; ++index) {
      if (fixture_pieces[index] && pointer==fixture_pieces[index]) {
        CHECK(!fixture_piece_freed[index]);
        fixture_piece_freed[index]=1;
        fixture_piece_free_steps[index]=step;
        if (fixture_mode!=0) {
          CHECK(!fixture_outer && fixture_callbacks==2);
          if (index==0) CHECK(fixture_piece_freed[1]);
          else CHECK(index==1 && !fixture_piece_freed[0]);
        }
        break;
      }
    }
  }
  // Other actual frees belong to array storage, closure env and capture cells.
  // Do not invent their layouts or skip them: the unchanged ledger tracks all.
}
static void fixture_observe_sources(uintptr_t seed,uintptr_t input,size_t length) {
  CHECK(seed && input && length==3);
  CHECK(!fixture_seed_freed && !fixture_input_freed);
  CHECK(__ku_sync_return_signal.kind==KU_SYNC_EXIT_NONE);
  if (!fixture_source_reads) {
    CHECK(!fixture_armed && !__ku_handler_timed_out && !__ku_handler_cleanup_deadline);
    fixture_seed=seed; fixture_input=input;
    CHECK(seed!=input);
  } else {
    CHECK(fixture_source_reads==1 && fixture_outer==1 && fixture_mode!=2);
    CHECK(seed==fixture_seed && input==fixture_input);
    if (fixture_mode==1) {
      CHECK(fixture_piece_freed[0] && fixture_piece_freed[1]);
      CHECK(fixture_piece_free_steps[1]<fixture_piece_free_steps[0]);
      CHECK(fixture_piece_free_steps[0]<fixture_step);
      CHECK(__ku_handler_timed_out && __ku_handler_unwind_depth>0);
      CHECK(__ku_handler_deadline==101 && __ku_handler_cleanup_deadline==1101);
    }
  }
  fixture_source_reads++;
}
static void fixture_observe_print(unsigned phase,uintptr_t pointer,uint8_t storage) {
  CHECK(phase && !ku_perf_overflow);
  ++fixture_step;
  CHECK(__ku_sync_return_signal.kind==KU_SYNC_EXIT_NONE);
  if (fixture_mode==1 && fixture_armed && phase!=3) {
    CHECK(__ku_handler_timed_out && __ku_handler_unwind_depth>0);
    CHECK(__ku_handler_deadline==101 && __ku_handler_cleanup_deadline==1101);
  }
  switch (phase) {
    case 1: CHECK(fixture_source_reads>0); break;
    case 2:
      CHECK(storage==KU_STRING_OWNED && pointer);
      if (pointer==fixture_seed) {
        CHECK(!fixture_seed_freed && fixture_capture_reads<2);
        fixture_capture_reads++;
      } else {
        CHECK(fixture_callbacks>=1 && fixture_callbacks<=3);
        unsigned index=fixture_callbacks-1;
        CHECK(!fixture_pieces[index] && pointer!=fixture_input);
        for (unsigned prior=0; prior<index; ++prior) {
          CHECK(pointer!=fixture_pieces[prior] && !fixture_piece_freed[prior]);
        }
        fixture_pieces[index]=pointer;
      }
      break;
    case 3:
      CHECK(!fixture_armed && fixture_source_reads==1 && fixture_capture_reads==1);
      fixture_armed=1;
      break;
    case 4: case 5: case 6:
      CHECK(fixture_armed && fixture_callbacks==phase-4);
      CHECK(fixture_mode==0 || phase!=6);
      fixture_callbacks++;
      break;
    case 7:
      CHECK(fixture_mode!=0 && fixture_callbacks==1 && !fixture_math_ok);
      fixture_math_ok++;
      break;
    case 8:
      CHECK(fixture_mode==0 && fixture_callbacks==3 && !fixture_map_complete);
      fixture_map_complete++;
      break;
    case 9:
      CHECK(fixture_mode!=2 && !fixture_outer);
      CHECK(!fixture_seed_freed && !fixture_input_freed);
      if (fixture_mode==1) CHECK(fixture_piece_freed[0] && fixture_piece_freed[1]);
      fixture_outer++;
      break;
    case 10:
      CHECK(fixture_mode==0 && fixture_outer==1 && fixture_source_reads==2 && !fixture_after_outer);
      fixture_after_outer++;
      break;
    default: CHECK(0);
  }
}
static void fixture_case(unsigned mode,const char* expected) {
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  CHECK(!__ku_call_depth && !__ku_handler_unwind_depth);
  CHECK(!__ku_handler_timed_out && !__ku_handler_deadline && !__ku_handler_cleanup_deadline);
  CHECK(__ku_sync_return_signal.kind==KU_SYNC_EXIT_NONE);
  __ku_sync_reset();
  fixture_mode=mode;
  fixture_armed=fixture_source_reads=fixture_capture_reads=0;
  fixture_callbacks=fixture_math_ok=fixture_map_complete=fixture_outer=fixture_after_outer=0;
  fixture_clock_reads=fixture_selection_reads=fixture_grace_reads=0;
  fixture_step=fixture_seed_freed=fixture_input_freed=0;
  fixture_seed=fixture_input=0;
  memset(fixture_pieces,0,sizeof(fixture_pieces));
  memset(fixture_piece_freed,0,sizeof(fixture_piece_freed));
  memset(fixture_piece_free_steps,0,sizeof(fixture_piece_free_steps));
  fixture_trace_len=0; fixture_trace[0]=0;
  size_t before=ku_perf_calls;
  if (mode==1) __ku_handler_timeout_begin(1);
  KuResult_null result=MapCase(mode==1,mode!=0);
  KuSyncExitSignal signal=__ku_sync_take();
  CHECK(__ku_sync_return_signal.kind==KU_SYNC_EXIT_NONE);
  CHECK(!strcmp(fixture_trace,expected));
  CHECK(fixture_seed_freed==1 && fixture_input_freed==1);
  CHECK(fixture_piece_freed[0] && fixture_piece_freed[1]);
  CHECK(ku_perf_calls-before>=7); /* actual owners plus input/output/env/cells */
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  CHECK(!__ku_call_depth && !__ku_handler_unwind_depth);
  fixture_empty_error(result.error);
  if (mode==0) {
    CHECK(signal.kind==KU_SYNC_EXIT_NONE && result.ok && result.value==0);
    CHECK(fixture_callbacks==3 && fixture_piece_freed[2] && !fixture_math_ok);
    CHECK(fixture_map_complete==1 && fixture_after_outer==1);
  } else {
    CHECK(signal.kind==(mode==1 ? KU_SYNC_EXIT_CLEANUP_ABORT : KU_SYNC_EXIT_ARITHMETIC_FATAL));
    CHECK(signal.arithmetic_status==KU_INT_DIV_ZERO);
    CHECK(!strcmp(__ku_sync_error_message(signal),"division by zero"));
    CHECK(!result.ok && result.value==0);
    CHECK(fixture_callbacks==2 && !fixture_pieces[2] && !fixture_piece_freed[2]);
    CHECK(fixture_math_ok==1 && !fixture_map_complete && !fixture_after_outer);
    CHECK(fixture_piece_free_steps[1]<fixture_piece_free_steps[0]);
  }
  CHECK(fixture_source_reads==(mode==2 ? 1u : 2u));
  CHECK(fixture_capture_reads==fixture_source_reads);
  CHECK(fixture_outer==(mode==2 ? 0u : 1u));
  if (mode==1) {
    CHECK(fixture_selection_reads==1 && fixture_grace_reads>0);
    CHECK(__ku_handler_timed_out && __ku_handler_deadline==101 && __ku_handler_cleanup_deadline==1101);
  } else CHECK(!__ku_handler_timed_out && !__ku_handler_deadline && !__ku_handler_cleanup_deadline);
  ku_result_drop_null(&result);
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes);
  CHECK(__ku_handler_timeout_finish()==(mode==1));
  CHECK(__ku_sync_take().kind==KU_SYNC_EXIT_NONE);
}
int main(void) {
  for (unsigned round=0; round<8; ++round) {
    fixture_case(0,"sources|capture-owner|armed|one|capture-owner|two|capture-owner|three|capture-owner|map-complete|outer|sources|capture-owner|after-outer|");
    fixture_case(1,"sources|capture-owner|armed|one|capture-owner|math-ok|two|capture-owner|outer|sources|capture-owner|");
    fixture_case(2,"sources|capture-owner|armed|one|capture-owner|math-ok|two|capture-owner|");
  }
  fputs("sync-map-closure-ok\n",stdout);
  return 0;
}
"#;

#[test]
fn native_sync_map_failure_drops_partial_owned_output_and_preserves_shared_sources() {
    let ast = Parser::new(Lexer::new(SOURCE).lex().expect("map source lexes"))
        .parse_program()
        .expect("map source parses");
    Checker::new()
        .check(&ast)
        .unwrap_or_else(|error| panic!("{error}\n{SOURCE}"));
    let lowered = ir::lower_program(&ast).expect("map source lowers within budget");
    let generated = c::generate_c_source(&ir::optimize_program(&lowered)).expect("native C emits");
    assert_eq!(generated.matches("static uint32_t ku_int_div(").count(), 1);
    assert!(
        generated.matches("ku_int_div(").count() > 1,
        "do not compile/run the old raw-C division path"
    );
    assert!(
        generated.contains("KuSyncExitSignal") && generated.contains("mapper.invoke(mapper.env,")
    );
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
    let sources = "void ReadSources(const KuString* seed, const KuArray_int* values) {";
    let generated = replace_once(generated,sources,&format!("{sources}\n  fixture_observe_sources((uintptr_t)seed->ptr,(uintptr_t)values->data,values->len);"));
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
    let directory = TempDir::new("native-sync-map-closure");
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
    .expect("map closure execution obeys the real process watchdog");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let round=concat!(
        "sources\ncapture-owner\n1\n3\narmed\none\ncapture-owner\ntwo\ncapture-owner\nthree\ncapture-owner\nmap-complete\n3\nouter\nsources\ncapture-owner\n1\n3\nafter-outer\n",
        "sources\ncapture-owner\n1\n3\narmed\none\ncapture-owner\nmath-ok\ntwo\ncapture-owner\nouter\nsources\ncapture-owner\n1\n3\n",
        "sources\ncapture-owner\n1\n3\narmed\none\ncapture-owner\nmath-ok\ntwo\ncapture-owner\n",
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        round.repeat(8) + "sync-map-closure-ok\n"
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
