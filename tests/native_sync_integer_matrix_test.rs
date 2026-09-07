//! Actual synchronous generated functions compared with Rust checked
//! arithmetic. A helper-emission gate prevents executing the old raw C UB.
#[path = "support/native_allocation_harness.rs"]
mod native_allocation_harness;
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

use ku::{backend::c, checker::Checker, ir, lexer::Lexer, parser::Parser};
use native_allocation_harness::ALLOCATION_HOOK;
use native_harness::{compile_harness, run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};
use std::{fmt::Write as _, fs, process::Command};

const SOURCE: &str = r#"
fn CheckedAdd(a: int, b: int): int { return a + b }
fn CheckedSub(a: int, b: int): int { return a - b }
fn CheckedMul(a: int, b: int): int { return a * b }
fn CheckedDiv(a: int, b: int): int { return a / b }
fn CheckedRem(a: int, b: int): int { return a % b }
fn CheckedNeg(a: int): int { return -a }
fn main() {}
"#;

fn c_int(value: i64) -> String {
    if value == i64::MIN {
        "INT64_MIN".into()
    } else if value < 0 {
        format!("(-INT64_C({}))", -value)
    } else {
        format!("INT64_C({value})")
    }
}

#[test]
fn native_sync_integer_functions_match_checked_oracle_without_allocating() {
    let ast = Parser::new(Lexer::new(SOURCE).lex().unwrap())
        .parse_program()
        .unwrap();
    Checker::new().check(&ast).unwrap();
    let program = ir::lower_program(&ast).unwrap();
    let generated = c::generate_c_source(&ir::optimize_program(&program)).unwrap();
    for helper in ["neg", "add", "sub", "mul", "div", "rem"] {
        assert_eq!(
            generated
                .matches(&format!("static uint32_t ku_int_{helper}("))
                .count(),
            1
        );
        assert!(
            generated.matches(&format!("ku_int_{helper}(")).count() > 1,
            "checked helper must be called before any native oracle run"
        );
    }
    assert!(generated.contains("KuSyncExitSignal"));
    assert!(!generated.contains("run_source") && !generated.contains("const SOURCE"));
    assert_eq!(generated.matches("typedef struct KuString {").count(), 1);
    let generated = generated.replacen(
        "typedef struct KuString {",
        &format!("{ALLOCATION_HOOK}\ntypedef struct KuString {{"),
        1,
    );
    assert_eq!(generated.matches("int main(void) {").count(), 1);
    let generated = generated.replacen(
        "int main(void) {",
        "static int fixture_unused_main(void) {",
        1,
    );

    let values = [
        i64::MIN,
        i64::MIN + 1,
        -3_037_000_500,
        -3_037_000_499,
        -2,
        -1,
        0,
        1,
        2,
        3_037_000_499,
        3_037_000_500,
        i64::MAX - 1,
        i64::MAX,
    ];
    let mut cases = String::new();
    let mut count = 0;
    for left in values {
        for right in values {
            for (operation, expected) in [
                left.checked_add(right),
                left.checked_sub(right),
                left.checked_mul(right),
                left.checked_div(right),
                left.checked_rem(right),
            ]
            .into_iter()
            .enumerate()
            {
                let status = if expected.is_some() {
                    0
                } else if operation >= 3 && right == 0 {
                    2
                } else {
                    1
                };
                writeln!(
                    cases,
                    "{{{operation}u, {}, {}, {status}u, {}}},",
                    c_int(left),
                    c_int(right),
                    c_int(expected.unwrap_or(0))
                )
                .unwrap();
                count += 1;
            }
        }
        let expected = left.checked_neg();
        let status = u32::from(expected.is_none());
        writeln!(
            cases,
            "{{5u, {}, 0, {status}u, {}}},",
            c_int(left),
            c_int(expected.unwrap_or(0))
        )
        .unwrap();
        count += 1;
    }
    assert_eq!(count, 858);
    let fixture = C_MAIN.replace("__CASES__", &cases);
    let directory = TempDir::new("native-sync-integer-oracle");
    let c_path = directory.path().join("program.c");
    fs::write(&c_path, format!("{generated}\n{fixture}")).unwrap();
    let Some(executable) = compile_harness(directory.path(), &c_path, "program") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI requires a real C compiler"
        );
        return;
    };
    fs::remove_file(c_path).unwrap();
    let output = run_bounded(
        Command::new(executable).current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("finite oracle must finish before its process watchdog");
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        "sync-integer-oracle-858-ok\n"
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

const C_MAIN: &str = r#"
#define CHECK(c) do { if (!(c)) { fprintf(stderr,"sync integer oracle line %d: %s\n",__LINE__,#c); abort(); } } while (0)
typedef struct FixtureCase { unsigned operation; int64_t left,right; unsigned status; int64_t expected; } FixtureCase;
static const FixtureCase fixture_cases[]={ __CASES__ };
static void fixture_clean(void) {
  CHECK(!__ku_call_depth && !__ku_handler_unwind_depth);
  CHECK(!__ku_handler_timed_out && !__ku_handler_deadline && !__ku_handler_cleanup_deadline);
  CHECK(!ku_perf_calls && !ku_perf_live_allocations && !ku_perf_live_bytes && !ku_perf_overflow);
  CHECK(__ku_sync_return_signal.kind==KU_SYNC_EXIT_NONE);
}
int main(void) {
  fixture_clean();
  CHECK(sizeof(fixture_cases)/sizeof(fixture_cases[0])==858u);
  for (size_t index=0;index<sizeof(fixture_cases)/sizeof(fixture_cases[0]);index++) {
    const FixtureCase* item=&fixture_cases[index];
    __ku_sync_reset();
    int64_t result=0;
    switch(item->operation) {
      case 0: result=CheckedAdd(item->left,item->right); break;
      case 1: result=CheckedSub(item->left,item->right); break;
      case 2: result=CheckedMul(item->left,item->right); break;
      case 3: result=CheckedDiv(item->left,item->right); break;
      case 4: result=CheckedRem(item->left,item->right); break;
      case 5: result=CheckedNeg(item->left); break;
      default: CHECK(0);
    }
    // Treat a failed return as private transport only after the root consumes
    // its signal; a zero result alone is not evidence of successful execution.
    KuSyncExitSignal signal=__ku_sync_take();
    CHECK(signal.arithmetic_status==item->status);
    CHECK(signal.kind==(item->status ? KU_SYNC_EXIT_ARITHMETIC_FATAL : KU_SYNC_EXIT_NONE));
    CHECK(result==item->expected);
    if (item->status) CHECK(!strcmp(__ku_sync_error_message(signal),item->status==2 ? "division by zero" : "integer overflow"));
    fixture_clean();
  }
  fputs("sync-integer-oracle-858-ok\n",stdout);
  return 0;
}
"#;
