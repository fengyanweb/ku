//! Check-only HTTP capture contracts for left-to-right argument evaluation.
//! Every case first accepts the same source with only registration removed.
//! No Ku program, native compiler, HTTP server or database is executed here.

use ku::{cli::check_source, lexer::Lexer, parser::Parser};

const SHARE_POINT: &str = "/* HTTP_SHARE_POINT */";
const REGISTER: &str = "app.get(\"/\", fn() { render(); return http.text(\"ok\") })";

fn source(setup: &str, call: &str) -> String {
    format!(
        r#"
import "std.http"
fn Noop(): null {{ return null }}
fn Other(): null {{ return null }}
fn InvokeAfter(ignored: null, action: fn(): null): null {{
    action()
    return null
}}
fn InvokeBefore(action: fn(): null, ignored: null): null {{
    action()
    return null
}}
fn main(): null! {{
    app = http.service()
    render = Noop
    op = Noop
    setter = () => {{ render = Other; return null }}
    install = () => {{ op = setter.clone(); return null }}
    // Pin the value immediately before the argument-evaluation boundary.
    op = Noop
    {setup}
    /* HTTP_SHARE_POINT */
    {call}
    return ok(null)
}}
"#
    )
}

fn parses(name: &str, source: &str) {
    let tokens = Lexer::with_file(name, source)
        .lex()
        .unwrap_or_else(|error| panic!("{name}: fixture must lex: {}", error.message));
    Parser::new(tokens)
        .parse_program()
        .unwrap_or_else(|error| panic!("{name}: fixture must parse: {}", error.message));
}

fn registered_after_legal_control(name: &str, setup: &str, call: &str) -> String {
    let source = source(setup, call);
    assert_eq!(source.matches(SHARE_POINT).count(), 1);
    let control_name = format!("control-{name}");
    let control = source.replace(SHARE_POINT, "");
    parses(&control_name, &control);
    check_source(&control_name, &control).unwrap_or_else(|error| {
        panic!(
            "{control_name}: legal control rejected: {}\n{control}",
            error.message
        )
    });
    let registered = source.replace(SHARE_POINT, REGISTER);
    parses(name, &registered);
    registered
}

fn rejects_later_argument(name: &str, setup: &str, call: &str) {
    let registered = registered_after_legal_control(name, setup, call);
    let error = check_source(name, &registered)
        .expect_err("the later argument must retain the newly installed setter effect");
    let writes_render = error
        .message
        .contains("cannot reassign HTTP-shared function binding 'render'");
    let explicit_shared_proof =
        error.message.contains("cannot prove") && error.message.contains("HTTP-shared");
    assert!(
        error.message.starts_with("error[E0704]:") && (writes_render || explicit_shared_proof),
        "{name}: expected E0704 for render or explicit HTTP-shared proof failure: {}\n{registered}",
        error.message
    );
}

fn accepts_earlier_argument(name: &str, setup: &str, call: &str) {
    let registered = registered_after_legal_control(name, setup, call);
    check_source(name, &registered).unwrap_or_else(|error| {
        panic!(
            "{name}: the already evaluated old function value must remain legal: {}\n{registered}",
            error.message
        )
    });
}

#[test]
fn http_shared_arguments_top_level_call_reads_installed_setter() {
    rejects_later_argument(
        "http-arguments-top-level-after.ku",
        "",
        "InvokeAfter(install(), op.clone())",
    );
}

#[test]
fn http_shared_arguments_function_value_call_reads_installed_setter() {
    rejects_later_argument(
        "http-arguments-function-value-after.ku",
        "callback = InvokeAfter",
        "callback(install(), op.clone())",
    );
}

#[test]
fn http_shared_arguments_temporary_callee_reads_installed_setter() {
    rejects_later_argument(
        "http-arguments-temporary-callee-after.ku",
        "callback = InvokeAfter",
        "callback.clone()(install(), op.clone())",
    );
}

#[test]
fn http_shared_arguments_top_level_call_keeps_old_function_value() {
    // The first argument is Noop. Installing the setter later changes op, not
    // the function value already passed to this invocation. Do not call op
    // afterward: that would be a separate, genuinely forbidden setter call.
    accepts_earlier_argument(
        "http-arguments-top-level-before.ku",
        "",
        "InvokeBefore(op.clone(), install())",
    );
}

#[test]
fn http_shared_arguments_function_value_call_keeps_old_function_value() {
    accepts_earlier_argument(
        "http-arguments-function-value-before.ku",
        "callback = InvokeBefore",
        "callback(op.clone(), install())",
    );
}

#[test]
fn http_shared_arguments_temporary_callee_keeps_old_function_value() {
    accepts_earlier_argument(
        "http-arguments-temporary-callee-before.ku",
        "callback = InvokeBefore",
        "callback.clone()(op.clone(), install())",
    );
}

#[test]
fn http_shared_destructure_later_setter_retains_shared_write_effect() {
    // Keep this installer outside the exact single-assignment/null-return rule.
    // Reset op again after definition-time checking of the replacement closure.
    let setup = r#"install = () => {
        op = setter.clone()
        marker = 0
        return null
    }
    op = Noop"#;
    // The existing helper first parses and accepts the otherwise identical
    // no-registration source, then parses the negative and requires E0704.
    rejects_later_argument(
        "http-destructure-setter-after-install.ku",
        setup,
        "_, selected = install(), op.clone()\n    selected()",
    );
}

#[test]
fn http_shared_destructure_earlier_callable_keeps_evaluated_value() {
    let setup = r#"install = () => {
        op = setter.clone()
        marker = 0
        return null
    }
    op = Noop"#;
    // Only RHS/destination order differs from the negative. The selected value
    // is Noop, not the setter installed into op by the later RHS evaluation.
    accepts_earlier_argument(
        "http-destructure-callable-before-install.ku",
        setup,
        "selected, _ = op.clone(), install()\n    selected()",
    );
}
