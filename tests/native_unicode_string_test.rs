//! Unicode transforms agree with Rust's locale-independent string semantics.
//! C inputs use explicit UTF-8 bytes, including NUL, so lexer escaping cannot
//! accidentally substitute the expected data for the value under test.
#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use ku::{backend::c, checker::Checker, ir, lexer::Lexer, parser::Parser};
use native_allocation_harness::ALLOCATION_HOOK;
use native_harness::{compile_harness, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};
use std::{fmt::Write, fs, process::Command};

fn generated(source: &str) -> String {
    let ast = Parser::new(Lexer::new(source).lex().expect("Unicode source lexes"))
        .parse_program()
        .expect("Unicode source parses");
    Checker::new().check(&ast).expect("Unicode source checks");
    let lowered = ir::lower_program(&ast).expect("Unicode source lowers");
    ir::verify_borrow_contract(&lowered).expect("transforms borrow input and own their output");
    c::generate_c_source(&ir::optimize_program(&lowered)).expect("Unicode C emits")
}

fn replace_once(source: String, anchor: &str, replacement: &str) -> String {
    assert_eq!(source.matches(anchor).count(), 1, "fixture anchor {anchor}");
    source.replacen(anchor, replacement, 1)
}

fn bytes_literal(value: &str) -> String {
    if value.is_empty() {
        return "{0}".into();
    }
    format!(
        "{{{}}}",
        value
            .bytes()
            .map(|byte| format!("{byte}u"))
            .collect::<Vec<_>>()
            .join(",")
    )
}

#[test]
fn native_unicode_helpers_are_emitted_only_when_used() {
    let code = generated("fn main() {println(\"plain\")}");
    for name in [
        "KuUnicodeRange",
        "ku_unicode_context",
        "ku_unicode_lower",
        "ku_unicode_upper",
        "ku_unicode_space",
        "ku_string_unicode_trim",
        "ku_string_unicode_case",
    ] {
        assert!(
            !code.contains(name),
            "unused Unicode implementation: {name}"
        );
    }
}

#[test]
fn native_unicode_transforms_match_rust_keep_input_and_release_timed_partial_output() {
    let source = r#"
fn Lower(&text:str):str { return text.lower() }
fn Upper(&text:str):str { return text.upper() }
fn Trim(&text:str):str { return text.trim() }
fn Temporary():str { return ("  İΣ"+"😀  ").trim().lower() }
fn AfterLower(&text:str):str! {
    result=text.lower()
    println("BAD-after-unicode-timeout")
    return ok(result)
}
fn main() {}
"#;
    let code = generated(source);
    let code = replace_once(
        code,
        "typedef struct KuString {",
        &format!("{ALLOCATION_HOOK}\nstatic unsigned fixture_clock;\ntypedef struct KuString {{"),
    );
    let start = code
        .find("static unsigned long long __ku_handler_now_ms(void) {")
        .expect("generated monotonic clock");
    let end = code
        .find("static void __ku_handler_timeout_begin(")
        .expect("generated deadline entry");
    assert!(start < end);
    let clock = "static unsigned long long __ku_handler_now_ms(void) {if(fixture_clock>64u){fputs(\"Unicode clock progress bound exceeded\\n\",stderr);abort();}return fixture_clock++;}\n";
    let code = format!("{}{clock}{}", &code[..start], &code[end..]);
    let mut code = replace_once(
        code,
        "int main(void) {",
        "static int fixture_unused_main(void) {",
    );
    let cases = [
        "",
        "Plain ASCII aAzZ09",
        " \t\r\n ",
        "\u{a0}\u{1680}\u{2000}\u{2009}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}",
        "\u{a0}中文😀\u{3000}",
        "\u{200b} zero width is not whitespace \u{feff}",
        "x\0Y",
        "\u{3000}\0\u{3000}",
        "Straße ßẞ",
        "İIıi",
        "e\u{301} Éé",
        "𐐀𐐨𞤀𞤢",
        "中文，emoji 🦀😀 unchanged",
        "Σ",
        "ΟΣ",
        "ΟΣΑ",
        "Α.Σ",
        "ΑΣ.Α",
        "Α\u{301}Σ",
        "ΑΣ\u{301}Α",
        "ΑΣ\u{301}.",
        "Α\u{345}Σ",
        "ΑΣ\u{345}Α",
        "\u{345}Σ",
        "AΣ'B",
        "AΣ'!",
        "AΣ\u{200d}B",
        "AΣ\u{200d}!",
    ];
    for (index, input) in cases.iter().enumerate() {
        for (kind, value) in [
            ("input", input.to_string()),
            ("lower", input.to_lowercase()),
            ("upper", input.to_uppercase()),
            ("trim", input.trim().to_string()),
        ] {
            writeln!(
                code,
                "static const uint8_t {kind}_{index}[]={};",
                bytes_literal(&value)
            )
            .unwrap();
        }
    }
    let temporary = "  İΣ😀  ".trim().to_lowercase();
    writeln!(
        code,
        "static const uint8_t temporary_expected[]={};",
        bytes_literal(&temporary)
    )
    .unwrap();
    code.push_str(
        r#"
#define CHECK(c) do {if(!(c)){fprintf(stderr,"Unicode fixture line %d: %s\n",__LINE__,#c);abort();}}while(0)
static void check_bytes(KuString actual,const uint8_t* expected,size_t length) {
    CHECK(actual.len==length);
    CHECK(!length || (actual.ptr && !memcmp(actual.ptr,expected,length)));
    CHECK(!length || actual.storage==KU_STRING_OWNED);
}
static void check_case(const uint8_t* bytes,size_t length,
                       const uint8_t* lower,size_t lower_length,
                       const uint8_t* upper,size_t upper_length,
                       const uint8_t* trim,size_t trim_length) {
    CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
    KuString input=ku_string_alloc(length);
    if(length)memcpy(input.ptr,bytes,length);
    KuString before=input;
    KuString a=Lower(&input), b=Upper(&input), c=Trim(&input);
    CHECK(input.ptr==before.ptr && input.len==before.len && input.capacity==before.capacity && input.storage==before.storage);
    check_bytes(input,bytes,length);
    if(a.len)CHECK(a.ptr!=input.ptr);
    if(b.len)CHECK(b.ptr!=input.ptr);
    if(c.len)CHECK(c.ptr!=input.ptr);
    ku_string_drop(&input);
    check_bytes(a,lower,lower_length);
    check_bytes(b,upper,upper_length);
    check_bytes(c,trim,trim_length);
    ku_string_drop(&a);ku_string_drop(&b);ku_string_drop(&c);
    CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
    CHECK(!__ku_call_depth && !__ku_handler_unwind_depth);
}
static void check_timed_transform(void) {
    KuString input=ku_string_alloc(192u);
    memset(input.ptr,'A',input.len);
    size_t live=ku_perf_live_bytes, calls=ku_perf_calls;
    fixture_clock=0;
    __ku_handler_timeout_begin(4u);
    // First pass: polls at 64/128/192. The fourth poll is in the
    // output pass after allocating the exact-size result buffer.
    KuString result=ku_string_unicode_lower(input);
    CHECK(__ku_handler_timed_out && fixture_clock==5u);
    CHECK(!result.ptr && !result.len && !result.capacity && !result.storage);
    CHECK(ku_perf_calls==calls+1u && ku_perf_live_bytes==live);
    CHECK(__ku_handler_timeout_finish()==1);
    for(size_t i=0;i<input.len;i++)CHECK(input.ptr[i]=='A');
    fixture_clock=0;
    __ku_handler_timeout_begin(4u);
    KuResult_str returned=AfterLower(&input);
    CHECK(__ku_handler_timed_out && !returned.ok && !returned.value.ptr);
    ku_result_drop_str(&returned);
    CHECK(ku_perf_live_bytes==live);
    CHECK(!__ku_call_depth && !__ku_handler_unwind_depth);
    CHECK(__ku_handler_timeout_finish()==1);
    ku_string_drop(&input);
    CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
}
int main(void) {
"#,
    );
    for (index, input) in cases.iter().enumerate() {
        writeln!(
            code,
            "  check_case(input_{index},{}u,lower_{index},{}u,upper_{index},{}u,trim_{index},{}u);",
            input.len(),
            input.to_lowercase().len(),
            input.to_uppercase().len(),
            input.trim().len()
        )
        .unwrap();
    }
    writeln!(code, "  KuString temporary=Temporary();check_bytes(temporary,temporary_expected,{}u);ku_string_drop(&temporary);", temporary.len()).unwrap();
    code.push_str("  CHECK(!ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);check_timed_transform();fputs(\"native-unicode-ok\\n\",stdout);return 0;}\n");

    let directory = TempDir::new("native-unicode-string");
    let path = directory.path().join("program.c");
    fs::write(&path, code).expect("write Unicode C");
    let Some(binary) = compile_harness(directory.path(), &path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires actual Unicode C execution"
        );
        return;
    };
    let output = run_bounded(
        Command::new(binary).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("Unicode execution obeys the existing watchdog");
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "native-unicode-ok\n"
    );
}
