//! Lexical bindings remain distinct when native control flow shares one C function.

#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use std::{fs, process::Command};

use ku::{backend::c, checker::Checker, cli::run_source, ir, lexer::Lexer, parser::Parser};
use native_harness::{compile_harness, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};

const SCOPE_SOURCE: &str = r#"
fn ScopeReturn(): str {
    try {
        value = "return" + " value"
        return value
    } finally {
        value = 17
        if (value != 17) panic("return finally local")
    }
    return "unexpected fallthrough"
}

fn ScopeBindings(): int {
    outer = 1
    err = "outer" + " error"
    try {
        ignored = 1
        if (ignored != 1) panic("first try local")
        outer = 7
    } catch (err) {
        panic(err.message)
    } finally {
        phase = "normal" + " finally"
        if (phase != "normal finally") panic("first finally local")
        outer += 1
    }
    try {
        ignored = "owned" + " value"
        if (ignored != "owned value") panic("second try local")
        fail "expected"
    } catch (err) {
        phase = 23
        if (err.message != "expected" || phase != 23) panic("catch local")
        outer += 1
    } finally {
        phase = ["last" + " finally"]
        if (phase[0] != "last finally") panic("second finally local")
        if (err != "outer error") panic("catch alias escaped into finally")
        outer += 1
    }
    try {
        err: int = 9
        if (err != 9) panic("explicit declaration must shadow the outer binding")
    } finally {
        if (err != "outer error") panic("try alias escaped")
    }
    selected: fn(): int = () => { return 0 }
    try {
        item = 31
        selected = () => { return item }
    } finally {
        item = "separate" + " binding"
        if (item != "separate binding") panic("finally local captured wrong binding")
    }
    try {
        item = ["another" + " binding"]
        if (item.len() != 1) panic("later try binding")
    } finally {
        if (selected() != 31) panic("closure lost its scoped cell")
    }
    ignored = ["outer" + " binding"]
    if (ignored[0] != "outer binding") panic("expired try alias remained visible")
    if (true) {
        branch = 1
        if (branch != 1) panic("then local")
    } else {
        branch = "other" + " branch"
        if (branch != "other branch") panic("else local")
    }
    sum = 0
    for part in [1, 2] { sum += part }
    for part in ["a", "b"] { sum += part.len() }
    rounds = 0
    while (rounds < 2) {
        scratch = "loop" + " value"
        if (scratch != "loop value") panic("while local")
        rounds += 1
    }
    returned = ScopeReturn()
    if (returned != "return value" || outer != 10 || err != "outer error" || sum != 5) {
        panic("scope exit changed an outer value or return")
    }
    return 42
}

fn main() { println(ScopeBindings()) }
"#;

const OWNED_FAIL_SOURCE: &str = r#"
fn OwnedMessage(): str {
    return "owned" + " call"
}

fn OwnedDomain(): str {
    return "owned." + "domain"
}

fn OwnedCode(): str {
    return "owned_" + "code"
}

fn OwnedErrorMessage(): str {
    return "owned " + "message"
}

fn OwnedErrMessage(): str {
    return "owned" + " err"
}

fn Mark(label: str, value: str): str {
    println(label)
    return "ordered." + value
}

fn FailOwnedLocal(): null! {
    message = "owned" + " local"
    fail message
}

fn FailOwnedCall(): null! {
    fail OwnedMessage()
}

fn ReturnOwnedErr(): null! {
    message = OwnedErrMessage()
    return err(message)
}

fn FailOwnedErrorLocals(): null! {
    domain = "owned." + "domain"
    code = "owned_" + "code"
    message = "owned " + "message"
    fail { domain: domain, code: code, message: message }
}

fn FailOwnedErrorCalls(): null! {
    fail {
        domain: OwnedDomain(),
        code: OwnedCode(),
        message: OwnedErrorMessage()
    }
}

fn FailOrderedErrorFields(): null! {
    fail {
        message: Mark("direct-message", "message"),
        domain: Mark("direct-domain", "domain"),
        code: Mark("direct-code", "code")
    }
}

fn RethrowOwnedError(use_calls: bool): null! {
    try {
        if (use_calls) {
            FailOwnedErrorCalls()?
        } else {
            FailOwnedErrorLocals()?
        }
    } catch(err) {
        fail err
    }
    return ok(null)
}

fn VerifyStringFailures(): null! {
    try {
        FailOwnedLocal()?
        panic("owned local did not fail")
    } catch(err) {
        if (err.domain != "ku" || err.code != "fail" || err.message != "owned local") {
            panic("owned local fail error was lost")
        }
    }
    try {
        FailOwnedCall()?
        panic("owned call did not fail")
    } catch(err) {
        if (err.domain != "ku" || err.code != "fail" || err.message != "owned call") {
            panic("owned call fail error was lost")
        }
    }
    try {
        message = "owned" + " try"
        fail message
    } catch(err) {
        if (err.domain != "ku" || err.code != "fail" || err.message != "owned try") {
            panic("owned try fail error was lost")
        }
    }
    try {
        ReturnOwnedErr()?
        panic("owned err did not fail")
    } catch(err) {
        if (err.domain != "ku" || err.code != "err" || err.message != "owned err") {
            panic("owned err result was lost")
        }
    }
    try {
        FailOrderedErrorFields()?
        panic("ordered direct Error did not fail")
    } catch(err) {
        if (err.domain != "ordered.domain" || err.code != "ordered.code" || err.message != "ordered.message") {
            panic("ordered direct Error field was lost")
        }
    }
    try {
        fail {
            message: Mark("try-message", "message"),
            domain: Mark("try-domain", "domain"),
            code: Mark("try-code", "code")
        }
    } catch(err) {
        if (err.domain != "ordered.domain" || err.code != "ordered.code" || err.message != "ordered.message") {
            panic("ordered try Error field was lost")
        }
    }
    return ok(null)
}

fn VerifyRethrow(use_calls: bool): null! {
    try {
        RethrowOwnedError(use_calls)?
        panic("owned Error did not fail")
    } catch(err) {
        if (err.domain != "owned.domain" || err.code != "owned_code" || err.message != "owned message") {
            panic("owned Error field was lost")
        }
        println(err.domain)
        println(err.code)
        println(err.message)
    } finally {
        println("finally")
    }
    return ok(null)
}

fn main(): null! {
    round = 0
    while (round < 8) {
        VerifyStringFailures()?
        VerifyRethrow(false)?
        VerifyRethrow(true)?
        round += 1
    }
    return ok(null)
}
"#;

fn generated_c(source: &str) -> String {
    let tokens = Lexer::new(source).tokenize().expect("lex scope fixture");
    let program = Parser::new(tokens).parse().expect("parse scope fixture");
    Checker::new().check(&program).expect("check scope fixture");
    let lowered = ir::lower_program(&program).expect("lower scope fixture");
    let bindings = lowered
        .functions
        .iter()
        .find(|function| function.name == "ScopeBindings")
        .expect("lowered scope function")
        .blocks
        .iter()
        .flat_map(|block| &block.instructions)
        .filter_map(|instruction| match instruction {
            ir::IrInst::Let { name, ty, .. } if name.ends_with("_ignored") => Some((name, ty)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        bindings.len(),
        2,
        "both try locals need distinct declarations"
    );
    assert_ne!(bindings[0].0, bindings[1].0);
    assert_ne!(bindings[0].1, bindings[1].1);
    c::generate_c_source(&ir::optimize_program(&lowered)).expect("emit scope C")
}

fn generated_owned_fail_c(source: &str) -> String {
    let tokens = Lexer::new(source)
        .tokenize()
        .expect("lex owned fail fixture");
    let program = Parser::new(tokens)
        .parse()
        .expect("parse owned fail fixture");
    Checker::new()
        .check(&program)
        .expect("check owned fail fixture");
    let lowered = ir::lower_program(&program).expect("lower owned fail fixture");
    let mut jump_errs = 0;
    for function in &lowered.functions {
        for block in &function.blocks {
            if let ir::IrTerminator::JumpErr { result, .. } = &block.terminator {
                jump_errs += 1;
                assert!(
                    matches!(
                        &result.kind,
                        ir::IrExprKind::Local(_) | ir::IrExprKind::Temp(_)
                    ),
                    "JumpErr in {} block{} would evaluate its Result more than once: {result:?}",
                    function.name,
                    block.id.0
                );
            }
        }
    }
    assert!(jump_errs > 0, "owned fail fixture must exercise JumpErr");
    c::generate_c_source(&ir::optimize_program(&lowered)).expect("emit owned fail C")
}

#[test]
fn native_scope_try_catch_finally_keep_local_names_and_outer_assignments_distinct() {
    run_source("native-scope.ku", SCOPE_SOURCE).expect("interpreter lexical scope fixture");
    let generated = generated_c(SCOPE_SOURCE);
    assert!(!generated.contains("run_source") && !generated.contains("const SOURCE"));
    let temp = TempDir::new("scope-bindings");
    let source = temp.path().join("scope.c");
    fs::write(&source, generated).expect("write scope C");
    let Some(executable) = compile_harness(temp.path(), &source, "scope") else {
        return;
    };
    let output = run_bounded(
        Command::new(executable).current_dir(temp.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .unwrap_or_else(|error| panic!("scope executable was not bounded: {error}"));
    assert!(
        output.status.success(),
        "scope executable failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout)
            .expect("scope stdout is UTF-8")
            .replace('\r', ""),
        "42\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn native_fail_materializes_owned_errors_before_cleaning_scope() {
    run_source("native-owned-fail.ku", OWNED_FAIL_SOURCE).expect("interpreter owned fail fixture");

    let generated = generated_owned_fail_c(OWNED_FAIL_SOURCE);
    assert!(!generated.contains("run_source") && !generated.contains("const SOURCE"));
    let temp = TempDir::new("owned-fail");
    let source = temp.path().join("owned-fail.c");
    fs::write(
        &source,
        format!(
            "{ALLOCATION_HOOKS}\n{generated}\n#undef main\n\
             int main(void) {{\n\
               int status = scope_generated_main();\n\
               if (scope_live_allocations != 0) {{\n\
                 fprintf(stderr, \"owned fail allocations did not balance: %lld\\n\", scope_live_allocations);\n\
                 return 2;\n\
               }}\n\
               return status;\n\
             }}\n"
        ),
    )
    .expect("write owned fail C");
    let Some(executable) = compile_harness(temp.path(), &source, "owned-fail") else {
        return;
    };
    let output = run_bounded(
        Command::new(executable).current_dir(temp.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .unwrap_or_else(|error| panic!("owned fail executable was not bounded: {error}"));
    assert!(
        output.status.success(),
        "owned fail executable failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let one_round = concat!(
        "direct-message\n",
        "direct-domain\n",
        "direct-code\n",
        "try-message\n",
        "try-domain\n",
        "try-code\n",
        "owned.domain\nowned_code\nowned message\nfinally\n",
        "owned.domain\nowned_code\nowned message\nfinally\n",
    );
    assert_eq!(
        String::from_utf8(output.stdout)
            .expect("owned fail stdout is UTF-8")
            .replace('\r', ""),
        one_round.repeat(8)
    );
    assert!(output.stderr.is_empty());
}

const ALLOCATION_HOOKS: &str = r#"
#include <stdlib.h>
#include <stdio.h>
static long long scope_live_allocations = 0;
static void* scope_malloc(size_t size) {
    void* value = malloc(size);
    if (value) ++scope_live_allocations;
    return value;
}
static void* scope_calloc(size_t count, size_t size) {
    void* value = calloc(count, size);
    if (value) ++scope_live_allocations;
    return value;
}
static void* scope_realloc(void* value, size_t size) {
    int was_null = value == NULL;
    void* result = realloc(value, size);
    if (result && was_null) ++scope_live_allocations;
    return result;
}
static void scope_free(void* value) {
    if (value) --scope_live_allocations;
    if (scope_live_allocations < 0) {
        fputs("scoped ownership freed an unowned allocation\n", stderr);
        exit(2);
    }
    free(value);
}
#define malloc scope_malloc
#define calloc scope_calloc
#define realloc scope_realloc
#define free scope_free
#define main scope_generated_main
"#;

#[test]
fn native_scope_owned_bindings_and_scoped_closure_cells_release_after_every_call() {
    let generated = generated_c(SCOPE_SOURCE);
    let temp = TempDir::new("scope-ownership");
    let source = temp.path().join("scope-ownership.c");
    fs::write(
        &source,
        format!(
            "{ALLOCATION_HOOKS}\n{generated}\n#undef main\n\
             int main(void) {{\n\
               for (int i = 0; i < 256; ++i) {{\n\
                 if (ScopeBindings() != 42 || scope_live_allocations != 0) {{\n\
                   fprintf(stderr, \"scoped ownership did not balance: %lld\\n\", scope_live_allocations);\n\
                   return 2;\n\
                 }}\n\
               }}\n\
               return 0;\n\
             }}\n"
        ),
    )
    .expect("write scope ownership harness");
    let Some(executable) = compile_harness(temp.path(), &source, "scope-ownership") else {
        return;
    };
    let output = run_bounded(
        Command::new(executable).current_dir(temp.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .unwrap_or_else(|error| panic!("scope ownership executable was not bounded: {error}"));
    assert!(
        output.status.success(),
        "scope ownership failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
}
