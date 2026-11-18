//! Synchronous loop exits use the real IR, native owner table and finally CFG.
//! The existing bounded harness supplies the C compiler and process watchdog.
#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use ku::{backend::c, checker::Checker, cli::run_source, ir, lexer::Lexer, parser::Parser};
use native_allocation_harness::ALLOCATION_HOOK;
use native_harness::{compile_harness, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};
use std::{fs, process::Command};

fn generate(source: &str) -> String {
    let ast = Parser::new(Lexer::new(source).lex().expect("loop source lexes"))
        .parse_program()
        .expect("loop source parses");
    Checker::new()
        .check(&ast)
        .unwrap_or_else(|error| panic!("{error}\n{source}"));
    let lowered = ir::lower_program(&ast).expect("synchronous loop control lowers");
    ir::verify_borrow_contract(&lowered).expect("loop cleanup preserves borrowed owners");
    c::generate_c_source(&ir::optimize_program(&lowered)).expect("loop C emits")
}

fn replace_once(source: String, anchor: &str, replacement: &str) -> String {
    assert_eq!(source.matches(anchor).count(), 1, "fixture anchor {anchor}");
    source.replacen(anchor, replacement, 1)
}

fn allocations(source: String) -> String {
    replace_once(
        source,
        "typedef struct KuString {",
        &format!("{ALLOCATION_HOOK}\ntypedef struct KuString {{"),
    )
}

fn execute(name: &str, source: String) -> Option<String> {
    let directory = TempDir::new(name);
    let path = directory.path().join("program.c");
    fs::write(&path, source).expect("write loop C");
    let Some(binary) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires actual C loop execution"
        );
        return None;
    };
    let output = run_bounded(
        Command::new(binary).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("loop execution obeys the existing process watchdog");
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    Some(String::from_utf8(output.stdout).unwrap().replace('\r', ""))
}

#[test]
fn native_nested_loop_exits_preserve_iterable_evaluation_and_finally_order() {
    let source = r#"
fn ReturnOverride():int {
    for i in 3 {
        try { return 7 }
        finally { if(i==0) {continue} break }
    }
    return 9
}
fn BreakOverride():int {
    while(true) {
        try { break }
        finally { return 11 }
    }
    return 12
}
fn NestedFinally():str {
    trace=""
    index=0
    while(index<3) {
        index+=1
        try {
            try { continue }
            finally { trace+="I" }
        } finally { trace+="O" }
    }
    return trace
}
fn main():null! {
    calls=0
    make=()=>{ calls+=1 return [1,2,3] }
    index=0
    total=0
    while(index<5) {
        index+=1
        if(index==2) {continue}
        for value in make() {
            if(value==2) {continue}
            total+=index*value
            if(index==4) {break}
        }
        if(index==4) {break}
    }
    if(index!=4 || total!=20 || calls!=3) {panic("nested loop or repeated iterable")}
    if(ReturnOverride()!=9 || BreakOverride()!=11 || NestedFinally()!="IOIOIO") {
        panic("finally did not override or forward loop control exactly once")
    }
    // Catch bindings do not escape into the finally's lexical name view.
    message="outer"
    while(true) {
        try { fail "inner" }
        catch(message) { break }
        finally { if(message!="outer") {panic("finally captured catch shadow")} }
    }
    // Break retires the existing ForEach snapshot before the same site reenters.
    rounds=0
    while(rounds<4) {
        rounds+=1
        for text in ["owned-"+"one","owned-"+"two"] {
            if(text!="owned-one") {panic("stale foreach snapshot")}
            break
        }
    }
    println("native-loop-order-ok")
    return ok(null)
}
"#;
    run_source("native-loop-order.ku", source).expect("interpreter loop/finally contract");
    let generated = allocations(generate(source));
    let mut generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_original_main(void) {",
    );
    generated.push_str("\nint main(void) {int status=fixture_original_main();if(status)return status;if(ku_perf_live_allocations || ku_perf_live_bytes || ku_perf_overflow){fputs(\"loop owners retained\\n\",stderr);return 91;}return 0;}\n");
    if let Some(output) = execute("native-loop-order", generated) {
        assert_eq!(output, "native-loop-order-ok\n");
    }
}

#[test]
fn native_loop_scope_owners_are_released_before_the_next_iteration() {
    let large = "x".repeat(16_384);
    let source = format!(
        r#"
fn main() {{
    kept="outer-"+"owner"
    index=0
    while(index<40) {{
        index+=1
        payload="{large}"+"!"
        read=()=>{{return payload.len()}}
        if(read()!=16385 || kept!="outer-owner") {{panic("loop owner prematurely released")}}
        if(index<40) {{continue}}
        break
    }}
    if(kept!="outer-owner") {{panic("outer owner lost")}}
    println("loop-scope-empty")
}}
"#
    );
    run_source("native-loop-owners.ku", &source).expect("interpreter loop owner contract");
    let generated = allocations(generate(&source));
    let generated = replace_once(
        generated,
        "static void ku_string_write(FILE* stream, KuString value) {",
        "static void ku_string_write(FILE* stream, KuString value) {\n  if(stream==stdout && value.len==16 && !memcmp(value.ptr,\"loop-scope-empty\",16)) {if(ku_perf_live_bytes>256 || ku_perf_peak_bytes>131072) {fputs(\"loop scope retained payload or closure\\n\",stderr);abort();}}",
    );
    let mut generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_original_main(void) {",
    );
    generated.push_str("\nint main(void) {int status=fixture_original_main();if(status)return status;if(ku_perf_live_allocations || ku_perf_live_bytes || ku_perf_overflow){fputs(\"loop owner cleanup incomplete\\n\",stderr);return 92;}return 0;}\n");
    if let Some(output) = execute("native-loop-owners", generated) {
        assert_eq!(output, "loop-scope-empty\n");
    }
}

#[test]
fn native_for_capture_keeps_distinct_cells_and_retires_abandoned_iterations() {
    let source = r#"
fn Stop(): null! { fail "stopped" }
fn Readers(): [fn(): str]! {
    readers: [fn(): str] = []
    name = "outer"
    round = 0
    while (round < 3) {
        round += 1
        try {
            for name in ["skip", "first" + str(round), "second" + str(round), "unused"] {
                if (name == "skip") { continue }
                read: fn(): str = () => { return name.clone() }
                try { Stop()? } catch(err) {
                    if (err.message != "stopped" || read() != name) {
                        panic("local catch lost the active iterator")
                    }
                }
                readers = readers.push(read)
                name = name + "!"
                if (round < 3) { Stop()? }
                if (name == "second3!") { break }
            }
        } catch(err) {
            if (err.message != "stopped") { panic("wrong loop error") }
        }
    }
    if (name != "outer") { panic("loop binding escaped its scope") }
    return ok(readers)
}
fn AfterFinally(): int {
    rounds = 0
    while (rounds < 3) {
        rounds += 1
        try {
            for i in 2 {
                read: fn(): int = () => i
                return read()
            }
        } finally {
            if (rounds < 3) { continue }
        }
    }
    return -1
}
fn main(): null! {
    readers = Readers()?
    if (readers.len() != 4) { panic("wrong reader count") }
    if (readers[0]() != "first1!" || readers[1]() != "first2!"
        || readers[2]() != "first3!" || readers[3]() != "second3!") {
        panic("iteration cells were shared or dropped")
    }
    if (AfterFinally() != 0) { panic("finally left a stale iterator") }
    println("native-for-capture-ok")
    return ok(null)
}
"#;
    run_source("native-for-capture.ku", source).expect("interpreter loop capture contract");
    let generated = allocations(generate(source));
    let mut generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_original_main(void) {",
    );
    generated.push_str(
        r#"
int main(void) {
    int status = fixture_original_main();
    if (status) return status;
    if (ku_perf_live_allocations || ku_perf_live_bytes || ku_perf_overflow) {
        fputs("captured loop owners retained\n", stderr);
        return 93;
    }
    return 0;
}
"#,
    );
    if let Some(output) = execute("native-for-capture", generated) {
        assert_eq!(output, "native-for-capture-ok\n");
    }
}

#[test]
fn native_continue_keeps_safepoints_and_finally_cannot_escape_selected_timeout() {
    let source = r#"
fn TimedWhile():str! {
    for outer in 1 {
        try { while(true) {continue} }
        finally {
            println("timed-while")
            hits=0
            for local in 2 {hits+=1 continue}
            if(hits!=2) {panic("local cleanup loop was suppressed")}
            try {break} finally {println("timed-while-inner")}
        }
    }
    return ok("BAD-after-while")
}
fn TimedFor():str! {
    while(true) {
        try {
            for index in 1000000000 {
                read: fn(): int = () => index
                continue
            }
        }
        finally {
            println("timed-for")
            try {continue} finally {println("timed-for-inner")}
        }
    }
    return ok("BAD-after-for")
}
fn main() {}
"#;
    let generated = allocations(generate(source));
    let generated = replace_once(
        generated,
        "typedef struct KuString {",
        "static unsigned fixture_reads;\ntypedef struct KuString {",
    );
    let start = generated
        .find("static unsigned long long __ku_handler_now_ms(void) {")
        .expect("real generated deadline clock");
    let end = generated
        .find("static void __ku_handler_timeout_begin(")
        .expect("real generated timeout entry");
    assert!(start < end);
    let clock = "static unsigned long long __ku_handler_now_ms(void) {if(++fixture_reads>256) {fputs(\"loop safepoint progress bound exceeded\\n\",stderr);abort();}if(__ku_handler_cleanup_deadline)return 600;return fixture_reads<8?100:101;}\n";
    let generated = format!("{}{clock}{}", &generated[..start], &generated[end..]);
    let mut generated = replace_once(
        generated,
        "int main(void) {",
        "static int fixture_unused_main(void) {",
    );
    generated.push_str(
        r#"
#define CHECK(c) do {if(!(c)){fprintf(stderr,"loop timeout line %d: %s\n",__LINE__,#c);abort();}}while(0)
typedef KuResult_str (*LoopCase)(void);
static void check_case(LoopCase function) {
    fixture_reads=0;
    __ku_handler_timeout_begin(1);
    KuResult_str result=function();
    CHECK(!result.ok && !result.value.ptr && !result.value.storage);
    CHECK(__ku_handler_timed_out && __ku_handler_deadline==101);
    CHECK(__ku_handler_cleanup_deadline==1101);
    ku_result_drop_str(&result);
    CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
    CHECK(!__ku_call_depth && !__ku_handler_unwind_depth);
    CHECK(__ku_handler_timeout_finish()==1);
    CHECK(!__ku_handler_deadline && !__ku_handler_cleanup_deadline);
}
int main(void) {
    check_case(TimedWhile);
    check_case(TimedFor);
    fputs("native-loop-timeout-ok\n",stdout);
    return 0;
}
"#,
    );
    if let Some(output) = execute("native-loop-timeout", generated) {
        assert_eq!(
            output,
            "timed-while\ntimed-while-inner\ntimed-for\ntimed-for-inner\nnative-loop-timeout-ok\n"
        );
    }
}
