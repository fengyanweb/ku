//! Templates parse normal Ku expressions and must keep their lazy bool rules.
#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod process_harness;

use ku::cli::check_source;
use process_harness::{run_bounded, BoundedOutput, TempDir, RUN_LIMITS, RUN_TIMEOUT};
use std::{fs, process::Command};

fn run(source: &str) -> BoundedOutput {
    // Checking explicitly proves the runtime cases use supported source syntax,
    // not malformed ASTs or an unchecked evaluator-only back door.
    check_source("template.ku", source).unwrap_or_else(|error| panic!("{error}\n{source}"));
    let directory = TempDir::new("template-short-circuit");
    let entry = directory.path().join("main.ku");
    fs::write(&entry, source).unwrap();
    run_bounded(
        Command::new(env!("CARGO_BIN_EXE_ku"))
            .args(["run", "main.ku"])
            .current_dir(directory.path()),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("template CLI run obeys the real process watchdog")
}

fn stdout(output: &BoundedOutput) -> String {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .replace('\r', "")
}

#[test]
fn template_logical_short_circuit_matches_normal_expression_order() {
    let expressions = [
        "true || false",
        "true || (1 / 0 == 0)",
        "false && (1 / 0 == 0)",
        "true && Mark(\"and\", true)",
        "false || Mark(\"or\", false)",
        "Mark(\"left\", false) || (Mark(\"middle\", true) && !Mark(\"right\", false))",
        "Mark(\"short\", true) || Mark(\"BAD\", false)",
        "false && Mark(\"BAD\", true)",
    ];
    let expected =
        "true\ntrue\nfalse\nand\ntrue\nor\nfalse\nleft\nmiddle\nright\ntrue\nshort\ntrue\nfalse\n";
    for template in [false, true] {
        let mut source = String::from(
            "fn Mark(label: str, value: bool): bool { println(label) return value }\nfn main() {\n",
        );
        for expression in expressions {
            if template {
                source.push_str(&format!("println(`{{{expression}}}`)\n"));
            } else {
                source.push_str(&format!("println({expression})\n"));
            }
        }
        source.push_str("}\n");
        let output = run(&source);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(stdout(&output), expected, "template={template}");
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn template_logical_operators_preserve_template_concat_and_check_skipped_rhs() {
    let output = run(r#"
fn main() {
    println(`{true && (1 + "px" == "1px")}`)
    println(`{false || ("v" + 1 == "v1")}`)
}
"#);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout(&output), "true\ntrue\n");
    assert!(output.stderr.is_empty());
    for expression in ["false && 1", "true || \"yes\"", "true || (1 - \"x\" == 0)"] {
        let source = format!("fn main() {{ println(`{{{expression}}}`) }}");
        let error = check_source("template.ku", &source)
            .expect_err("short-circuiting does not bypass static RHS checking")
            .to_string();
        assert!(error.contains("type error"), "{error}");
    }
}

#[test]
fn template_required_arithmetic_error_stays_fatal_without_panicking() {
    for expression in ["true && (1 / 0 == 0)", "false || (1 / 0 == 0)"] {
        let source = format!(
            r#"
fn main() {{
    try {{
        println(`{{{expression}}}`)
        println("BAD after expression")
    }} catch (err) {{
        println("BAD caught fatal error")
    }}
    println("BAD after try")
}}
"#
        );
        let output = run(&source);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty(), "{}", stdout(&output));
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains("division by zero"), "{error}");
        assert!(
            !error.contains("panicked") && !error.contains("unreachable"),
            "{error}"
        );
    }
}

#[test]
fn template_logical_question_failure_stops_remaining_evaluation() {
    let output = run(r#"
fn Fail(): bool! { return err("expected") }
fn Mark(): bool { println("BAD evaluated") return true }
fn main() {
    try {
        println(`{Fail()? || Mark()}`)
        println("BAD continued after left failure")
    } catch (err) {
        println(err.message)
    }
    try {
        println(`{true && Fail()?}`)
        println("BAD continued after right failure")
    } catch (err) {
        println(err.message)
    }
    try {
        println(`{true || Fail()?}`)
    } catch (err) {
        println("BAD skipped failure was evaluated")
    }
}
"#);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout(&output), "expected\nexpected\ntrue\n");
    assert!(output.stderr.is_empty());
}
