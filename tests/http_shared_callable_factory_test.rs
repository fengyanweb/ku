//! Check-only HTTP handler factories and evaluated function-value snapshots.
//! Negative controls remove only app.get, keeping the complete factory call.
//! These fixtures never run Ku, start a server or invoke a native compiler.

use ku::{
    ast::{Item, ModuleDecl},
    checker::Checker,
    cli::check_source,
    error::{DiagnosticId, KuError},
    lexer::Lexer,
    parser::Parser,
};

fn source(expression: &str, registered: bool) -> String {
    let expression = if registered {
        format!("app.get(\"/\", {expression})")
    } else {
        expression.to_string()
    };
    format!(
        r#"
import "std.http"
fn After<T>(ignored: null, value: T): T {{ return value }}
fn Before<T>(value: T, ignored: null): T {{ return value }}
fn Identity<T>(value: T): T {{ return value }}
fn main(): null! {{
    app = http.service()
    count = 0
    handler = () => {{ return http.text("safe") }}
    bad = () => {{ count += 1; return http.text("bad") }}
    install = () => {{
        handler = bad.clone()
        marker = 0
        return null
    }}
    // Reset the value after definition-time checking. The marker above keeps
    // install outside the exact single-assignment/literal-null transfer rule.
    handler = () => {{ return http.text("safe") }}
    {expression}
    return ok(null)
}}
"#
    )
}

fn check_raw(name: &str, source: &str) -> Result<(), KuError> {
    let tokens = Lexer::with_file(name, source)
        .lex()
        .unwrap_or_else(|error| panic!("{name}: fixture must lex: {}", error.message));
    let mut program = Parser::new(tokens)
        .parse_program()
        .unwrap_or_else(|error| panic!("{name}: fixture must parse: {}", error.message));
    // Retain the structured checker diagnostic. Use ModuleLoader's exact
    // std:http marker and also check the real CLI/import path separately.
    let mut imports = 0;
    for item in &mut program.items {
        if let Item::Import(import) = item {
            assert_eq!(import.path, "std.http");
            imports += 1;
            *item = Item::Module(ModuleDecl {
                name: "std:http".to_string(),
                span: import.span,
            });
        }
    }
    assert_eq!(imports, 1, "one std.http import per fixture");
    Checker::new().check(&program)
}

fn accepts(name: &str, source: &str) {
    check_raw(name, source)
        .unwrap_or_else(|error| panic!("{name}: legal source rejected: {error}\n{source}"));
    check_source(name, source)
        .unwrap_or_else(|error| panic!("{name}: CLI rejected legal source: {error}\n{source}"));
}

fn rejects_factory(name: &str, expression: &str) {
    // Do not erase installation or use a different callback in the control.
    // The same expression must be legal when not registered as an HTTP route.
    accepts(&format!("control-{name}"), &source(expression, false));
    rejects_registered(name, &source(expression, true));
}

fn rejects_registered(name: &str, registered: &str) {
    let error = check_raw(name, registered)
        .expect_err("HTTP must reject the installed mutator or explicitly fail its safety proof");
    let shared_proof = error.diagnostic_id() == DiagnosticId::HttpSharedCallableReassignment
        && error.message.contains("cannot prove")
        && error.message.contains("HTTP-shared");
    let actual_mutation = error.diagnostic_id() == DiagnosticId::HttpCapturedMutation
        && error
            .message
            .contains("http handler cannot modify captured variable 'count'");
    assert!(
        shared_proof || actual_mutation,
        "{name}: expected relevant E0704 or count's E0703, not another rejection: {error}\n{registered}"
    );
    let data = error.diagnostic_data(name, registered);
    let expected_code = if shared_proof { "E0704" } else { "E0703" };
    assert_eq!(data.code, expected_code);
    assert!(data.line > 0 && data.column > 0, "real source location");
    let cli_error = check_source(name, registered).expect_err("CLI must reject the same factory");
    assert!(
        cli_error
            .message
            .starts_with(&format!("error[{expected_code}]:"))
            && cli_error.message.contains(&error.message),
        "{name}: CLI diagnostic differs from raw checker: {cli_error}"
    );
}

#[test]
fn http_shared_factory_later_argument_cannot_register_installed_mutator() {
    rejects_factory(
        "http-factory-after-install.ku",
        "After(install(), handler.clone())",
    );
}

#[test]
fn http_shared_factory_nested_identity_cannot_erase_installation_evidence() {
    rejects_factory(
        "http-factory-nested-after-install.ku",
        "Identity(After(install(), handler.clone()))",
    );
}

#[test]
fn http_shared_factory_without_installation_remains_legal() {
    accepts(
        "http-factory-safe.ku",
        &source("After(null, handler.clone())", true),
    );
}

#[test]
fn http_shared_factory_earlier_argument_keeps_the_evaluated_safe_value() {
    // The old safe function value was cloned before install changes handler.
    // Register that value, not the binding's new contents; do not call handler
    // afterward because doing so would be a distinct invocation of the mutator.
    accepts(
        "http-factory-before-install.ku",
        &source("Before(handler.clone(), install())", true),
    );
}

#[test]
fn http_shared_factory_cloned_result_keeps_installation_evidence() {
    rejects_factory(
        "http-factory-cloned-after-install.ku",
        "After(install(), handler.clone()).clone()",
    );
}

#[test]
fn http_shared_factory_stored_result_keeps_installation_evidence() {
    let name = "http-factory-stored-after-install.ku";
    // Only the final app.get wrapper differs. Both paths evaluate and retain
    // precisely the same factory result before reading the stored binding.
    accepts(
        &format!("control-{name}"),
        &source(
            "stored = After(install(), handler.clone())\n    stored",
            false,
        ),
    );
    rejects_registered(
        name,
        &source(
            "stored = After(install(), handler.clone())\n    app.get(\"/\", stored)",
            false,
        ),
    );
}

#[test]
fn http_shared_factory_cloned_earlier_result_keeps_the_safe_value() {
    accepts(
        "http-factory-cloned-before-install.ku",
        &source("Before(handler.clone(), install()).clone()", true),
    );
}

#[test]
fn http_shared_factory_stored_earlier_result_keeps_the_safe_value() {
    accepts(
        "http-factory-stored-before-install.ku",
        &source(
            "stored = Before(handler.clone(), install())\n    app.get(\"/\", stored)",
            false,
        ),
    );
}

#[test]
fn http_shared_destructure_later_handler_retains_installation_evidence() {
    let name = "http-destructure-handler-after-install.ku";
    let registration = r#"app.get("/", chosen)"#;
    let registered = source(
        r#"_, chosen = install(), handler.clone()
    app.get("/", chosen)"#,
        false,
    );
    assert_eq!(registered.matches(registration).count(), 1);
    // Remove only registration: retain the installer and both RHS evaluations.
    // accepts/check_raw lex and parse independently before checker assertions.
    let control = registered.replace(registration, "");
    accepts(&format!("control-{name}"), &control);
    rejects_registered(name, &registered);
}

#[test]
fn http_shared_destructure_earlier_handler_keeps_evaluated_safe_value() {
    // Stores happen after all RHS evaluations, but chosen owns the safe value
    // cloned before install. Do not re-read the later contents of handler.
    accepts(
        "http-destructure-handler-before-install.ku",
        &source(
            r#"chosen, _ = handler.clone(), install()
    app.get("/", chosen)"#,
            false,
        ),
    );
}
