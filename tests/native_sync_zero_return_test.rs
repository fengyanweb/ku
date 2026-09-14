//! Private cleanup-zero returns share one epilogue instead of reserving one
//! aggregate return scratch per arithmetic guard. This is a bounded artifact
//! regression plus real C ownership execution, not a second reproduction of
//! the complete Stage-3 stack-overflow witness.
#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use ku::{
    backend::c,
    checker::Checker,
    ir::{self, IrExprKind, IrTerminator},
    lexer::Lexer,
    parser::Parser,
};
use native_allocation_harness::ALLOCATION_HOOK;
use native_harness::{compile_harness, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};
use std::{fmt::Write, fs, process::Command};

const FIELDS: usize = 32;
const ADDS: usize = 256;

fn source() -> String {
    let mut source = String::from("struct Bundle {\n");
    for field in 0..FIELDS {
        writeln!(source, "    field{field}: str").unwrap();
    }
    source.push_str("}\nfn Work(owner: Bundle, seed: int, step: int): Bundle! {\n    n = seed\n");
    for _ in 0..ADDS {
        source.push_str("    n = n + step\n");
    }
    writeln!(
        source,
        "    if (n != {ADDS}) {{ panic(\"BAD-normal-arithmetic\") }}"
    )
    .unwrap();
    source.push_str("    return ok(owner)\n}\nfn main() {}\n");
    source
}

fn replace_once(source: String, anchor: &str, replacement: &str) -> String {
    assert_eq!(
        source.matches(anchor).count(),
        1,
        "zero-return anchor: {anchor}"
    );
    source.replacen(anchor, replacement, 1)
}

const OBSERVERS: &str = r#"
static unsigned fixture_entered,fixture_adds;
static uintptr_t fixture_owners[32];
static unsigned fixture_freed[32];
static void fixture_free_observed(void*);
"#;

const C_MAIN: &str = r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr,"zero-return line %d: %s\n",__LINE__,#c); abort(); } } while (0)
static void fixture_free_observed(void* pointer) {
  if (!pointer) return;
  uintptr_t identity=(uintptr_t)pointer;
  unsigned found=0;
  for (unsigned index=0;index<32;++index) {
    if (fixture_owners[index]==identity) {
      CHECK(!fixture_freed[index]);
      fixture_freed[index]=1; found++;
    }
  }
  CHECK(found==1);
}
static KuString fixture_string(unsigned index) {
  CHECK(index<32 && !fixture_owners[index]);
  uint8_t* bytes=(uint8_t*)malloc(8);
  CHECK(bytes);
  for (unsigned offset=0;offset<8;++offset) bytes[offset]=(uint8_t)(index+offset+1);
  fixture_owners[index]=(uintptr_t)bytes;
  return (KuString){bytes,8,8,KU_STRING_OWNED};
}
static int fixture_empty_string(KuString value) {
  return !value.ptr && !value.len && !value.capacity && !value.storage;
}
static void fixture_field(KuString value,unsigned index) {
  CHECK(index<32 && !fixture_freed[index]);
  CHECK((uintptr_t)value.ptr==fixture_owners[index]);
  CHECK(value.len==8 && value.capacity==8 && value.storage==KU_STRING_OWNED);
  for (unsigned offset=0;offset<8;++offset) CHECK(value.ptr[offset]==(uint8_t)(index+offset+1));
}
static void fixture_run(unsigned overflow) {
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  CHECK(!__ku_call_depth && !__ku_handler_unwind_depth);
  CHECK(!__ku_handler_timed_out && !__ku_handler_deadline && !__ku_handler_cleanup_deadline);
  CHECK(__ku_sync_return_signal.kind==KU_SYNC_EXIT_NONE);
  fixture_entered=fixture_adds=0;
  memset(fixture_owners,0,sizeof(fixture_owners)); memset(fixture_freed,0,sizeof(fixture_freed));
  size_t before=ku_perf_calls;
  KuStruct_Bundle input={0};
@INITIALIZE@
  CHECK(ku_perf_calls-before==32 && ku_perf_live_allocations==32 && ku_perf_live_bytes==256);
  __ku_sync_reset();
  // This is the real generated owned-parameter move, not an aliasing raw copy.
  KuResult_struct_Bundle result=Work(ku_move_struct_Bundle(&input),overflow ? INT64_MAX : 0,1);
  // Root protocol: take the actual return mailbox before any Result inspection.
  KuSyncExitSignal signal=__ku_sync_take();
  CHECK(signal.kind==(overflow ? KU_SYNC_EXIT_ARITHMETIC_FATAL : KU_SYNC_EXIT_NONE));
  CHECK(signal.arithmetic_status==(overflow ? KU_INT_OVERFLOW : 0));
  CHECK(__ku_sync_take().kind==KU_SYNC_EXIT_NONE);
  CHECK(fixture_entered==1 && fixture_adds==(overflow ? 1u : 256u));
  CHECK(!__ku_call_depth && !__ku_handler_unwind_depth);
  CHECK(!__ku_handler_timed_out && !__ku_handler_deadline && !__ku_handler_cleanup_deadline);
@INPUT_EMPTY@
  CHECK(result.ok==!overflow);
  CHECK(fixture_empty_string(result.error.domain));
  CHECK(fixture_empty_string(result.error.code));
  CHECK(fixture_empty_string(result.error.message));
  if (overflow) {
    CHECK(!strcmp(__ku_sync_error_message(signal),"integer overflow"));
@ERROR_EMPTY@
    for (unsigned index=0;index<32;++index) CHECK(fixture_freed[index]==1);
    CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes);
  } else {
@SUCCESS_FIELDS@
    CHECK(ku_perf_live_allocations==32 && ku_perf_live_bytes==256);
    for (unsigned index=0;index<32;++index) CHECK(!fixture_freed[index]);
  }
  // Both branches use the real generated Result drop; the error placeholder
  // owns nothing, while successful source Return moved all fields into result.
  ku_result_drop_struct_Bundle(&result);
  for (unsigned index=0;index<32;++index) CHECK(fixture_freed[index]==1);
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  CHECK(ku_perf_calls-before==32); // No cloning or new per-guard heap allocation.
}
int main(void) {
  for (unsigned round=0;round<4;++round) { fixture_run(0); fixture_run(1); }
  puts("private-zero-epilogue-ok");
  return 0;
}
"#;

#[test]
fn native_sync_private_zero_returns_share_epilogue_and_preserve_large_owned_result() {
    let source = source();
    let ast = Parser::new(Lexer::new(&source).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new()
        .check(&ast)
        .unwrap_or_else(|error| panic!("{error}\n{source}"));
    let lowered = ir::lower_program(&ast).unwrap();
    let optimized = ir::optimize_program(&lowered);
    let work = optimized
        .functions
        .iter()
        .find(|function| function.name == "Work")
        .expect("actual checked source Work function");
    let private_returns: Vec<_> = work
        .blocks
        .iter()
        .enumerate()
        .filter(|(_, block)| {
            matches!(
                &block.terminator,
                IrTerminator::Return(Some(value))
                    if matches!(&value.kind, IrExprKind::Literal(text) if text == "<native-zero>")
            )
        })
        .collect();
    assert_eq!(private_returns.len(), ADDS);
    assert_eq!(
        work.blocks
            .iter()
            .filter(|block| matches!(block.terminator, IrTerminator::SyncGuard { .. }))
            .count(),
        ADDS
    );

    let generated = c::generate_c_source(&optimized).unwrap();
    for forbidden in ["run_source", "const SOURCE"] {
        assert!(!generated.contains(forbidden));
    }
    let signature = generated
        .lines()
        .find(|line| line.starts_with("KuResult_struct_Bundle Work(") && line.ends_with(" {"))
        .expect("actual generated Work definition")
        .to_owned();
    let function_start = generated.find(&signature).unwrap();
    let function_end = function_start
        + generated[function_start..]
            .find("\n}\n")
            .expect("Work closing brace")
        + 3;
    let work_c = &generated[function_start..function_end];
    assert_eq!(work_c.matches("ku_int_add(").count(), ADDS);
    // Old artifacts fail here before compilation. This deliberately does not
    // execute an unsafe large-stack baseline or assert its exact frame size.
    for (index, block) in private_returns {
        assert!(block.instructions.is_empty(), "isolated private-zero block");
        let label = format!("block{}:;\n", block.id.0);
        let start = work_c.find(&label).expect("private-zero C block") + label.len();
        let end = work.blocks.get(index + 1).map_or_else(
            || work_c.find("__ku_sync_epilogue:;").unwrap(),
            |next| work_c.find(&format!("block{}:;\n", next.id.0)).unwrap(),
        );
        assert!(start < end);
        let body = &work_c[start..end];
        assert!(
            body.contains("goto __ku_sync_epilogue;"),
            "private cleanup zero must use the single epilogue"
        );
        assert!(!body.contains("__ku_return"));
        assert!(!body.contains("return "));
        assert!(!body.contains("(KuResult_struct_Bundle){"));
    }
    assert_eq!(
        work_c
            .matches("KuResult_struct_Bundle __ku_return =")
            .count(),
        1,
        "only the actual source Return keeps an owned return scratch"
    );
    assert_eq!(
        work_c
            .matches("return (KuResult_struct_Bundle){0};")
            .count(),
        1,
        "one shared native-zero compound return, not one per guard"
    );

    let generated = replace_once(
        generated,
        "typedef struct KuString {",
        &format!("{OBSERVERS}\n{ALLOCATION_HOOK}\ntypedef struct KuString {{"),
    );
    let generated = replace_once(
        generated,
        "static void ku_perf_free(void* value) {",
        "static void ku_perf_free(void* value) {\n  fixture_free_observed(value);",
    );
    let generated = replace_once(
        generated,
        "static uint32_t ku_int_add(int64_t left, int64_t right, int64_t* out) {",
        "static uint32_t ku_int_add(int64_t left, int64_t right, int64_t* out) {\n  fixture_adds++;",
    );
    let generated = replace_once(
        generated,
        &signature,
        &format!("{signature}\n  fixture_entered++;"),
    );
    let generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_unused_source_main(void) {",
    );
    let mut initialize = String::new();
    let mut input_empty = String::new();
    let mut error_empty = String::new();
    let mut success_fields = String::new();
    for field in 0..FIELDS {
        writeln!(initialize, "  input.field{field}=fixture_string({field});").unwrap();
        writeln!(
            input_empty,
            "  CHECK(fixture_empty_string(input.field{field}));"
        )
        .unwrap();
        writeln!(
            error_empty,
            "    CHECK(fixture_empty_string(result.value.field{field}));"
        )
        .unwrap();
        writeln!(
            success_fields,
            "    fixture_field(result.value.field{field},{field});"
        )
        .unwrap();
    }
    let main = C_MAIN
        .replace("@INITIALIZE@", &initialize)
        .replace("@INPUT_EMPTY@", &input_empty)
        .replace("@ERROR_EMPTY@", &error_empty)
        .replace("@SUCCESS_FIELDS@", &success_fields);
    let directory = TempDir::new("native-sync-private-zero-epilogue");
    let ku_path = directory.path().join("program.ku");
    let c_path = directory.path().join("program.c");
    fs::write(&ku_path, source).unwrap();
    fs::write(&c_path, format!("{generated}\n{main}")).unwrap();
    let Some(executable) = compile_harness(directory.path(), &c_path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires a real native C compiler"
        );
        return;
    };
    fs::remove_file(ku_path).unwrap();
    fs::remove_file(c_path).unwrap();
    // Reuse unchanged compiler options and the existing 20-second watchdog.
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("bounded private-zero native execution");
    assert!(
        output.status.success(),
        "private-zero fixture: {:?}\n{}{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "private-zero-epilogue-ok\n"
    );
    assert!(output.stderr.is_empty());
}
