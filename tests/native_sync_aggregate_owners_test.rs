//! One actual source frame retaining aggregate owners across checked arithmetic.
//! Ordinary fatal skips user catch/finally; an already-selected timeout still
//! permits its outer finally to borrow the original owners under the first D.
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
struct Record { text: str, tags: [str] }
enum Choice { Text(value: str) }
fn Problem(): str! {
    fail { domain: "agg-" + "domain", code: "agg-" + "code", message: "agg-" + "message" }
}
fn ChoiceText(value: Choice): str { return match value { Choice.Text(text) => text } }
fn Inspect(&success: str!, &failure: str!, &words: [str], &record: Record, &choice: Choice, &reader: fn(): int): null! {
    println("inspect")
    println(success.clone()?)
    try { failure.clone()? }
    catch (error) { println(error.domain) println(error.code) println(error.message) }
    println(words[0])
    println(record.text)
    println(record.tags[0])
    println(ChoiceText(choice.clone()))
    println(reader())
    return ok(null)
}
fn Divide(n: int, denominator: int): int {
    println("divide-enter")
    return n / denominator
}
fn Case(timed: bool, denominator: int): null! {
    success: str! = ok("ok-" + "payload")
    failure: str! = Problem()
    words = ["array-" + "payload"]
    object = { text: "object-" + "payload" }
    record = Record { text: "record-" + "payload", tags: ["tag-" + "payload"] }
    choice = Choice.Text("enum-" + "payload")
    stamp = 7
    reader = fn(): int { return stamp }
    try { Problem()? }
    catch (error) {
        Inspect(success, failure, words, record, choice, reader)?
        println(object.get_or("text", null))
        println("catch-fields")
        println(error.domain) println(error.code) println(error.message)
        println("armed")
        try { if (timed) { while (true) {} } }
        finally {
            try {
                quotient = Divide(12, denominator)
                if (quotient != 4) { panic("BAD-normal-quotient") }
                println("math-complete")
            } catch (unexpected) { println("BAD-math-catch") }
            finally {
                println("outer")
                Inspect(success, failure, words, record, choice, reader)?
                println(object.get_or("text", null))
                println("catch-fields")
                println(error.domain) println(error.code) println(error.message)
                println("read-complete")
            }
            println("after-outer")
        }
    }
    println("after-catch")
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
static unsigned fixture_mode,fixture_armed,fixture_inspections,fixture_object_reads;
static unsigned fixture_fields,fixture_fields_remaining,fixture_divides,fixture_math_complete;
static unsigned fixture_outer,fixture_read_complete,fixture_after_outer,fixture_after_catch;
static unsigned fixture_clock_reads,fixture_selections,fixture_grace_reads;
static uintptr_t fixture_owners[18];
static unsigned fixture_freed[18];
static void fixture_require(int condition);
static void fixture_owner(unsigned id,uintptr_t pointer);
static void fixture_observe_free(void* pointer);
static void fixture_observe_inspect(void);
static void fixture_observe_object(void);
static void fixture_observe_print(const uint8_t* pointer,size_t length,uint8_t storage);
"#;

// Only the clock is replaced. The generated poll selects timeout after armed;
// its real enter latches 1101. Cleanup remains at 600 under that unchanged D.
const CLOCK: &str = r#"
static unsigned long long __ku_handler_now_ms(void) {
  if (++fixture_clock_reads>512) {
    fputs("aggregate arithmetic clock bound exceeded\n",stderr); abort();
  }
  if (__ku_handler_cleanup_deadline) {
    fixture_require(__ku_handler_cleanup_deadline==1101);
    fixture_grace_reads++;
    return 600;
  }
  if (fixture_mode==2 && fixture_armed) {
    if (!__ku_handler_timed_out) fixture_selections++;
    return 101;
  }
  return 100;
}
"#;

// These are observations of the actual borrowed ABI, before Inspect clones or
// unwraps anything. No replacement allocation, drop, Result or success is used.
const INSPECT_HOOK: &str = r#"
  fixture_require(success->ok && !failure->ok && words->len==1 && record->tags.len==1 && choice->tag==0);
  fixture_owner(0,(uintptr_t)success->value.ptr);
  fixture_owner(1,(uintptr_t)failure->error.domain.ptr);
  fixture_owner(2,(uintptr_t)failure->error.code.ptr);
  fixture_owner(3,(uintptr_t)failure->error.message.ptr);
  fixture_owner(4,(uintptr_t)words->data);
  fixture_owner(5,(uintptr_t)words->data[0].ptr);
  fixture_owner(6,(uintptr_t)record->text.ptr);
  fixture_owner(7,(uintptr_t)record->tags.data);
  fixture_owner(8,(uintptr_t)record->tags.data[0].ptr);
  fixture_owner(9,(uintptr_t)choice->payload.Text.value.ptr);
  fixture_owner(10,(uintptr_t)reader->env);
  fixture_observe_inspect();
"#;

const OBJECT_HOOK: &str = r#"
  fixture_require(found && found->tag==KU_STR);
  fixture_owner(11,(uintptr_t)object);
  fixture_owner(12,(uintptr_t)object->entries);
  fixture_owner(13,(uintptr_t)found->as.s.ptr);
  fixture_observe_object();
"#;

const C_MAIN: &str = r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr,"aggregate arithmetic line %d: %s\n",__LINE__,#c); abort(); } } while (0)
static void fixture_require(int condition) { CHECK(condition); }
static void fixture_owner(unsigned id,uintptr_t pointer) {
  CHECK(id<18 && pointer && !fixture_freed[id]);
  if (fixture_owners[id]) CHECK(fixture_owners[id]==pointer);
  else {
    for (unsigned other=0; other<18; ++other) CHECK(!fixture_owners[other] || fixture_owners[other]!=pointer);
    fixture_owners[id]=pointer;
  }
}
static void fixture_all_alive(void) {
  for (unsigned id=0; id<18; ++id) CHECK(fixture_owners[id] && !fixture_freed[id]);
}
static void fixture_observe_free(void* pointer) {
  uintptr_t identity=(uintptr_t)pointer;
  CHECK(pointer && !ku_perf_overflow);
  for (unsigned id=0; id<18; ++id) {
    if (fixture_owners[id] && fixture_owners[id]==identity) {
      CHECK(!fixture_freed[id]);
      // Only ordinary fatal may release before the outer source reads. In the
      // other modes, those actual borrowed reads must complete first.
      CHECK(fixture_divides==1 && (fixture_mode==1 || fixture_read_complete==1));
      fixture_freed[id]=1;
      break;
    }
  }
}
static void fixture_observe_inspect(void) {
  CHECK(__ku_sync_return_signal.kind==KU_SYNC_EXIT_NONE && !ku_perf_overflow);
  CHECK(fixture_inspections<2);
  if (fixture_inspections) {
    CHECK(fixture_mode!=1 && fixture_outer==1);
    fixture_all_alive();
  } else CHECK(!fixture_armed && !fixture_outer);
  fixture_inspections++;
}
static void fixture_observe_object(void) {
  CHECK(fixture_object_reads<2 && fixture_inspections==fixture_object_reads+1);
  CHECK(__ku_sync_return_signal.kind==KU_SYNC_EXIT_NONE);
  fixture_object_reads++;
}
static void fixture_observe_print(const uint8_t* pointer,size_t length,uint8_t storage) {
  CHECK(__ku_sync_return_signal.kind==KU_SYNC_EXIT_NONE && !ku_perf_overflow);
  if (fixture_mode==2 && fixture_armed) {
    CHECK(__ku_handler_timed_out && __ku_handler_unwind_depth>0);
    CHECK(__ku_handler_deadline==101 && __ku_handler_cleanup_deadline==1101);
  }
  if (fixture_fields_remaining) {
    unsigned index=3-fixture_fields_remaining;
    static const char* fields[]={"agg-domain","agg-code","agg-message"};
    CHECK(storage==KU_STRING_OWNED && length==strlen(fields[index]));
    CHECK(!memcmp(pointer,fields[index],length));
    // An owned catch-field expression currently materializes a clone for
    // printing. Only content is observed here; it is not the Error owner.
    if (!--fixture_fields_remaining) fixture_fields++;
    return;
  }
#define IS(s) (length==sizeof(s)-1 && !memcmp(pointer,s,sizeof(s)-1))
  if (IS("catch-fields")) {
    CHECK(fixture_fields<2 && fixture_inspections==fixture_fields+1);
    fixture_fields_remaining=3;
  } else if (IS("armed")) {
    CHECK(!fixture_armed && fixture_inspections==1 && fixture_object_reads==1 && fixture_fields==1);
    fixture_all_alive();
    fixture_armed=1;
  } else if (IS("divide-enter")) {
    CHECK(fixture_armed && !fixture_divides);
    fixture_all_alive();
    fixture_divides=1;
  } else if (IS("math-complete")) {
    CHECK(fixture_mode==0 && fixture_divides==1 && !fixture_math_complete);
    fixture_math_complete=1;
  } else if (IS("outer")) {
    CHECK(fixture_mode!=1 && fixture_divides==1 && !fixture_outer);
    CHECK(fixture_math_complete==(fixture_mode==0));
    fixture_all_alive(); fixture_outer=1;
  } else if (IS("read-complete")) {
    CHECK(fixture_outer==1 && fixture_inspections==2 && fixture_object_reads==2 && fixture_fields==2);
    fixture_all_alive(); fixture_read_complete=1;
  } else if (IS("after-outer")) {
    CHECK(fixture_mode==0 && fixture_read_complete==1 && !fixture_after_outer);
    fixture_after_outer=1;
  } else if (IS("after-catch")) {
    CHECK(fixture_mode==0 && fixture_after_outer==1 && !fixture_after_catch);
    fixture_after_catch=1;
  }
#undef IS
}
static int fixture_empty_string(KuString value) {
  return !value.ptr && !value.len && !value.capacity && !value.storage;
}
static void fixture_run(unsigned mode) {
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  CHECK(!__ku_call_depth && !__ku_handler_unwind_depth);
  CHECK(!__ku_handler_timed_out && !__ku_handler_deadline && !__ku_handler_cleanup_deadline);
  CHECK(__ku_sync_return_signal.kind==KU_SYNC_EXIT_NONE);
  __ku_sync_reset();
  fixture_mode=mode;
  fixture_armed=fixture_inspections=fixture_object_reads=fixture_fields=fixture_fields_remaining=0;
  fixture_divides=fixture_math_complete=fixture_outer=fixture_read_complete=0;
  fixture_after_outer=fixture_after_catch=fixture_clock_reads=fixture_selections=fixture_grace_reads=0;
  memset(fixture_owners,0,sizeof(fixture_owners)); memset(fixture_freed,0,sizeof(fixture_freed));
  if (mode==2) __ku_handler_timeout_begin(1);
  KuResult_null result=Case(mode==2,mode==0 ? 3 : 0);
  // A real root consumes the signal before inspecting the placeholder Result.
  KuSyncExitSignal signal=__ku_sync_take();
  CHECK(signal.kind==(mode==0 ? KU_SYNC_EXIT_NONE : mode==1 ? KU_SYNC_EXIT_ARITHMETIC_FATAL : KU_SYNC_EXIT_CLEANUP_ABORT));
  if (mode) {
    CHECK(signal.arithmetic_status==KU_INT_DIV_ZERO);
    CHECK(!strcmp(__ku_sync_error_message(signal),"division by zero"));
  } else CHECK(!signal.arithmetic_status);
  CHECK(__ku_sync_take().kind==KU_SYNC_EXIT_NONE);
  CHECK(result.ok==(mode==0) && !result.value);
  CHECK(fixture_empty_string(result.error.domain));
  CHECK(fixture_empty_string(result.error.code));
  CHECK(fixture_empty_string(result.error.message));
  CHECK(fixture_armed==1 && fixture_divides==1 && !fixture_fields_remaining);
  CHECK(fixture_inspections==(mode==1 ? 1u : 2u));
  CHECK(fixture_object_reads==fixture_inspections && fixture_fields==fixture_inspections);
  CHECK(fixture_outer==(mode!=1) && fixture_read_complete==(mode!=1));
  CHECK(fixture_after_outer==(mode==0) && fixture_after_catch==(mode==0));
  for (unsigned id=0; id<18; ++id) CHECK(fixture_owners[id] && fixture_freed[id]==1);
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  CHECK(!__ku_call_depth && !__ku_handler_unwind_depth);
  if (mode==2) {
    CHECK(fixture_selections==1 && fixture_grace_reads>0);
    CHECK(__ku_handler_timed_out && __ku_handler_deadline==101 && __ku_handler_cleanup_deadline==1101);
  } else CHECK(!__ku_handler_timed_out && !__ku_handler_deadline && !__ku_handler_cleanup_deadline);
  ku_result_drop_null(&result);
  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  CHECK(__ku_handler_timeout_finish()==(mode==2));
  CHECK(!__ku_handler_deadline && !__ku_handler_cleanup_deadline && !__ku_handler_timed_out);
}
int main(void) {
  for (unsigned round=0; round<4; ++round) {
    fixture_run(0); fixture_run(1); fixture_run(2);
  }
  fputs("aggregate-owners-ok\n",stdout);
  return 0;
}
"#;

#[test]
fn native_sync_aggregate_owners_survive_cleanup_borrows_and_drop_once_on_all_math_exits() {
    let ast = Parser::new(Lexer::new(SOURCE).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new()
        .check(&ast)
        .unwrap_or_else(|error| panic!("{error}\n{SOURCE}"));
    let lowered = ir::lower_program(&ast).unwrap();
    let optimized = ir::optimize_program(&lowered);
    // Resolve the actual catch binding from the source-produced optimized IR,
    // not a guessed C temporary or the cloned value seen by println.
    let error_names: Vec<_> = optimized
        .functions
        .iter()
        .find(|function| function.name == "Case")
        .expect("actual Case source function")
        .blocks
        .iter()
        .flat_map(|block| &block.instructions)
        .filter_map(|instruction| match instruction {
            ir::IrInst::BindError { name, .. } if name == "error" || name.ends_with("_error") => {
                Some(name.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        error_names.len(),
        1,
        "one outer catch(error) owns these three fields"
    );
    let error_name = error_names[0];
    assert!(error_name
        .bytes()
        .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric()));
    let generated = c::generate_c_source(&optimized).unwrap();
    assert_eq!(generated.matches("static uint32_t ku_int_div(").count(), 1);
    assert!(
        generated.matches("ku_int_div(").count() > 1,
        "never execute old raw-C division"
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
    let generated = replace_once(generated, "static void ku_string_write(FILE* stream, KuString value) {",
        "static void ku_string_write(FILE* stream, KuString value) {\n  if (stream==stdout) fixture_observe_print(value.ptr,value.len,value.storage);");
    let signature = generated
        .lines()
        .find(|line| line.starts_with("KuResult_null Inspect(") && line.ends_with(" {"))
        .expect("actual typed borrow Inspect definition")
        .to_owned();
    let generated = replace_once(generated, &signature, &format!("{signature}{INSPECT_HOOK}"));
    // Register the original Error immediately after the real move into its
    // catch binding. Scope the anchor to Case so another function's error
    // binding cannot accidentally satisfy this observation.
    let case_signature = generated
        .lines()
        .find(|line| line.starts_with("KuResult_null Case(") && line.ends_with(" {"))
        .expect("actual Case definition")
        .to_owned();
    let case_start = generated.find(&case_signature).unwrap();
    // Object construction can emit an unindented closing brace inside a
    // function. Its unique frame epilogue, not the first brace, bounds the body.
    let case_end = case_start
        + generated[case_start..]
            .find("\n__ku_sync_epilogue:;\n")
            .expect("actual Case frame epilogue");
    let catch_assignment = format!("  {error_name} = __ku_store; }}\n");
    let observed_assignment = format!(
        "{catch_assignment}  fixture_owner(14,(uintptr_t){error_name}.domain.ptr);\n  fixture_owner(15,(uintptr_t){error_name}.code.ptr);\n  fixture_owner(16,(uintptr_t){error_name}.message.ptr);\n"
    );
    let case_body = replace_once(
        generated[case_start..case_end].to_owned(),
        &catch_assignment,
        &observed_assignment,
    );
    let generated = format!(
        "{}{case_body}{}",
        &generated[..case_start],
        &generated[case_end..]
    );
    let object = "static KuValue ku_object_get_or(KuObject* object, KuString key, KuValue fallback) {\n  KuValue* found = ku_object_get(object, key);";
    let generated = replace_once(generated, object, &format!("{object}{OBJECT_HOOK}"));
    // There is exactly one captured Copy int cell in this source. Preserve its
    // real allocation, initialized RC, release and payload; only record identity.
    let cell = "c->value = init; ku_atomic_refcount_init(&c->rc); return c;";
    let generated = replace_once(generated, cell,
        "c->value = init; ku_atomic_refcount_init(&c->rc); fixture_owner(17,(uintptr_t)c); return c;");
    let clock_start = "static unsigned long long __ku_handler_now_ms(void) {";
    let clock_end = "static void __ku_handler_timeout_begin(";
    assert_eq!(generated.matches(clock_start).count(), 1);
    assert_eq!(generated.matches(clock_end).count(), 1);
    let start = generated.find(clock_start).unwrap();
    let end = generated.find(clock_end).unwrap();
    assert!(start < end);
    let generated = format!("{}{CLOCK}{}", &generated[..start], &generated[end..]);
    let generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_unused_source_main(void) {",
    );
    let directory = TempDir::new("native-sync-aggregate-owners");
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
    .expect("aggregate owner execution obeys the real process watchdog");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let inspect = concat!(
        "inspect\nok-payload\nagg-domain\nagg-code\nagg-message\n",
        "array-payload\nrecord-payload\ntag-payload\nenum-payload\n7\n",
        "object-payload\ncatch-fields\nagg-domain\nagg-code\nagg-message\n"
    );
    let prefix = format!("{inspect}armed\ndivide-enter\n");
    let normal =
        format!("{prefix}math-complete\nouter\n{inspect}read-complete\nafter-outer\nafter-catch\n");
    let timed = format!("{prefix}outer\n{inspect}read-complete\n");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        (normal + &prefix + &timed).repeat(4) + "aggregate-owners-ok\n"
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
