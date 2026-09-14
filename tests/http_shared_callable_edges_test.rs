//! Flow/call-shape adversarial checks for HTTP-shared callable cells.
//! Each negative has a checked legal control with only registration removed.
//! No fixture runs Ku code, creates a server, or invokes a native compiler.

use ku::{
    ast::{Item, ModuleDecl},
    checker::Checker,
    cli::check_source,
    error::{DiagnosticId, KuError},
    lexer::Lexer,
    parser::Parser,
};

const SHARE: &str = "/* HTTP_SHARE_POINT */";
const REGISTER: &str = "app.get(\"/\", fn() { render(); return http.text(\"registered\") })";

fn program(body: &str) -> String {
    format!(
        r#"
import "std.http"
fn Noop(): null {{ return null }}
fn Other(): null {{ return null }}
fn Invoke(op: fn(): null): null {{ op(); return null }}
fn Erase(op: fn(): null): fn(): null {{ return op }}
fn main(): null! {{
    app = http.service()
    {body}
    return ok(null)
}}
"#
    )
}

fn check_raw(name: &str, source: &str) -> Result<(), KuError> {
    let tokens = Lexer::with_file(name, source)
        .lex()
        .unwrap_or_else(|error| panic!("{name}: fixture must lex: {}", error.message));
    let mut ast = Parser::new(tokens)
        .parse_program()
        .unwrap_or_else(|error| panic!("{name}: fixture must parse: {}", error.message));
    // Keep the original structured Checker diagnostic: check_source renders it
    // into a new KuError. These fixtures import only std.http; use exactly the
    // std:http marker emitted by ModuleLoader, and check the real CLI pipeline
    // separately below rather than treating this marker as import coverage.
    let mut imports = 0;
    for item in &mut ast.items {
        if let Item::Import(import) = item {
            assert_eq!(import.path, "std.http", "unexpected fixture import");
            imports += 1;
            *item = Item::Module(ModuleDecl {
                name: "std:http".to_string(),
                span: import.span,
            });
        }
    }
    assert_eq!(imports, 1, "each fixture has exactly its std.http import");
    Checker::new().check(&ast)
}

fn accepts(name: &str, body: &str) {
    let source = program(body);
    check_raw(name, &source)
        .unwrap_or_else(|error| panic!("{name}: legal control rejected: {error}\n{source}"));
    check_source(name, &source)
        .unwrap_or_else(|error| panic!("{name}: CLI rejected legal control: {error}\n{source}"));
}

fn rejects_after_sharing(name: &str, body: &str, allow_unproven: bool) {
    rejects_after_registration(name, body, REGISTER, allow_unproven, None);
}

fn rejects_after_registration(
    name: &str,
    body: &str,
    registration: &str,
    allow_unproven: bool,
    expected_write: Option<&str>,
) {
    assert_eq!(body.matches(SHARE).count(), 1, "one sharing boundary");
    // An unrelated syntax/type/ownership error must fail this control, not make
    // the negative test look successful. Keep every other statement unchanged.
    accepts(&format!("control-{name}"), &body.replace(SHARE, ""));
    let source = program(&body.replace(SHARE, registration));
    let error = check_raw(name, &source).expect_err("shared callable write must fail closed");
    assert_eq!(
        error.diagnostic_id(),
        DiagnosticId::HttpSharedCallableReassignment,
        "{name}: unrelated rejection: {error}\n{source}"
    );
    let data = error.diagnostic_data(name, &source);
    assert_eq!(data.code, "E0704", "{name}: {error}");
    assert!(data.line > 0 && data.column > 0, "{name}: real location");
    let reassignment = error
        .message
        .contains("cannot reassign HTTP-shared function binding 'render'");
    assert!(
        reassignment || (allow_unproven && error.message.contains("cannot prove HTTP-shared")),
        "{name}: expected render write or relevant proof diagnostic: {error}"
    );
    if reassignment {
        assert!(
            error.message.contains("HTTP registration at line"),
            "{name}: reassignment should retain registration evidence: {error}"
        );
    }
    if let Some(write) = expected_write {
        assert!(
            reassignment,
            "{name}: exact write needs a reassignment diagnostic"
        );
        assert_eq!(source.matches(write).count(), 1, "one expected assignment");
        assert_eq!(source.matches(registration).count(), 1, "one registration");
        let write_line = source
            .lines()
            .position(|line| line.contains(write))
            .unwrap()
            + 1;
        let register_line = source
            .lines()
            .position(|line| line.contains(registration))
            .unwrap()
            + 1;
        assert_eq!(error.span.start.line, write_line, "{name}: assignment site");
        assert!(error.span.start.column > 0, "{name}: assignment column");
        assert!(
            error
                .message
                .contains(&format!("HTTP registration at line {register_line},")),
            "{name}: missing registration evidence: {error}"
        );
    }
    let rendered = check_source(name, &source).expect_err("CLI must reject the same source");
    assert!(
        rendered.message.starts_with("error[E0704]:"),
        "{name}: CLI lost or changed the structured diagnostic: {rendered}"
    );
}

#[test]
fn http_shared_edges_for_callable_backedge_rechecks_captured_setter() {
    rejects_after_registration(
        "http-shared-for-callable-backedge.ku",
        r#"
    render = Noop
    helper = () => { render(); return http.text("ok") }
    setter = () => { render = Other; return null }
    for chosen in [helper.clone(), helper.clone()] {
        setter()
        /* HTTP_SHARE_POINT */
    }
"#,
        r#"app.get("/", chosen)"#,
        false,
        Some("render = Other"),
    );
}

#[test]
fn http_shared_edges_for_callable_continue_rechecks_captured_setter() {
    rejects_after_registration(
        "http-shared-for-callable-continue.ku",
        r#"
    render = Noop
    helper = () => { render(); return http.text("ok") }
    setter = () => { render = Other; return null }
    for chosen in [helper.clone(), helper.clone()] {
        setter()
        /* HTTP_SHARE_POINT */
        continue
    }
"#,
        r#"app.get("/", chosen)"#,
        false,
        Some("render = Other"),
    );
}

#[test]
fn http_shared_edges_for_callable_break_keeps_prior_setter_legal() {
    accepts(
        "http-shared-for-callable-break.ku",
        r#"
    render = Noop
    helper = () => { render(); return http.text("ok") }
    setter = () => { render = Other; return null }
    for chosen in [helper.clone(), helper.clone()] {
        setter()
        app.get("/", chosen)
        break
    }
    render()
"#,
    );
}

#[test]
fn http_shared_edges_escaped_setter_keeps_original_cell_evidence() {
    rejects_after_sharing(
        "http-shared-escaped-setter.ku",
        r#"
    escaped = Noop
    enabled = true
    if (enabled) {
        render = Noop
        escaped = () => { render = Other; return null }
        /* HTTP_SHARE_POINT */
    } else {
        return ok(null)
    }
    escaped()
"#,
        false,
    );
}

#[test]
fn http_shared_edges_escaped_cloned_setter_keeps_original_cell_evidence() {
    rejects_after_sharing(
        "http-shared-escaped-cloned-setter.ku",
        r#"
    escaped = Noop
    enabled = true
    if (enabled) {
        render = Noop
        setter = () => { render = Other; return null }
        escaped = setter.clone()
        /* HTTP_SHARE_POINT */
    } else {
        return ok(null)
    }
    again = escaped.clone()
    again()
"#,
        false,
    );
}

#[test]
fn http_shared_edges_temporary_cloned_setter_call_cannot_skip_effects() {
    rejects_after_sharing(
        "http-shared-temporary-setter.ku",
        r#"
    render = Noop
    setter = () => { render = Other; return null }
    /* HTTP_SHARE_POINT */
    setter.clone()()
"#,
        false,
    );
}

#[test]
fn http_shared_edges_typed_higher_order_invoke_cannot_hide_setter() {
    rejects_after_sharing(
        "http-shared-typed-invoke.ku",
        r#"
    render = Noop
    setter = () => { render = Other; return null }
    /* HTTP_SHARE_POINT */
    Invoke(setter.clone())
"#,
        false,
    );
}

#[test]
fn http_shared_edges_erased_setter_call_requires_proof() {
    rejects_after_sharing(
        "http-shared-erased-setter.ku",
        r#"
    render = Noop
    setter = () => { render = Other; return null }
    erased = Erase(setter.clone())
    /* HTTP_SHARE_POINT */
    erased()
"#,
        true,
    );
}

#[test]
fn http_shared_edges_throw_path_carries_freeze_into_catch() {
    rejects_after_sharing(
        "http-shared-catch-write.ku",
        r#"
    render = Noop
    try {
        /* HTTP_SHARE_POINT */
        fail "probe"
    } catch (err) {
        render = Other
    }
"#,
        false,
    );
}

#[test]
fn http_shared_edges_return_path_carries_freeze_into_finally() {
    rejects_after_sharing(
        "http-shared-return-finally.ku",
        r#"
    render = Noop
    enabled = true
    try {
        if (enabled) {
            /* HTTP_SHARE_POINT */
            return ok(null)
        }
    } finally {
        render = Other
    }
"#,
        false,
    );
}

#[test]
fn http_shared_edges_catch_registration_carries_freeze_into_finally() {
    rejects_after_sharing(
        "http-shared-catch-finally.ku",
        r#"
    render = Noop
    try {
        fail "probe"
    } catch (err) {
        /* HTTP_SHARE_POINT */
    } finally {
        render = Other
    }
"#,
        false,
    );
}

#[test]
fn http_shared_edges_loop_backedge_rechecks_predeclared_setter_effect() {
    rejects_after_sharing(
        "http-shared-setter-loop.ku",
        r#"
    render = Noop
    setter = () => { render = Other; return null }
    turn = 0
    while (turn < 2) {
        setter()
        if (turn == 0) {
            /* HTTP_SHARE_POINT */
        }
        turn += 1
    }
"#,
        false,
    );
}

#[test]
fn http_shared_edges_for_backedge_rechecks_predeclared_setter_effect() {
    rejects_after_sharing(
        "http-shared-for-setter-backedge.ku",
        r#"
    render = Noop
    setter = () => { render = Other; return null }
    for turn in [0, 1] {
        setter()
        if (turn == 0) {
            /* HTTP_SHARE_POINT */
        }
    }
"#,
        false,
    );
}

#[test]
fn http_shared_edges_for_continue_rechecks_predeclared_setter_effect() {
    rejects_after_sharing(
        "http-shared-for-setter-continue.ku",
        r#"
    render = Noop
    setter = () => { render = Other; return null }
    for turn in [0, 1] {
        setter()
        if (turn == 0) {
            /* HTTP_SHARE_POINT */
            continue
        }
        break
    }
"#,
        false,
    );
}

#[test]
fn http_shared_edges_for_break_before_backedge_keeps_prior_setter_legal() {
    accepts(
        "http-shared-for-setter-break.ku",
        r#"
    render = Noop
    setter = () => { render = Other; return null }
    for turn in [0, 1] {
        setter()
        app.get("/", fn() { render(); return http.text("ok") })
        break
    }
    render()
"#,
    );
}

#[test]
fn http_shared_edges_service_move_does_not_thaw_captured_binding() {
    rejects_after_sharing(
        "http-shared-after-service-move.ku",
        r#"
    render = Noop
    /* HTTP_SHARE_POINT */
    moved = app
    render = Other
"#,
        false,
    );
}

#[test]
fn http_shared_edges_service_scope_exit_does_not_thaw_outer_binding() {
    rejects_after_sharing(
        "http-shared-after-service-scope.ku",
        r#"
    render = Noop
    if (true) {
        /* HTTP_SHARE_POINT */
        scoped = app
    }
    render = Other
"#,
        false,
    );
}

#[test]
fn http_shared_edges_listener_close_does_not_thaw_captured_binding() {
    // bind/close is checked only; do not run this fixture or claim native
    // support for the interpreter-only listener lifecycle API.
    rejects_after_sharing(
        "http-shared-after-listener-close.ku",
        r#"
    render = Noop
    /* HTTP_SHARE_POINT */
    listener = app.bind("127.0.0.1:0")?
    listener.close()?
    render = Other
"#,
        false,
    );
}

#[test]
fn http_shared_edges_del_route_registration_does_not_clear_prior_freeze() {
    // del is the DELETE-method registration API, not a route-removal API.
    rejects_after_sharing(
        "http-shared-after-del.ku",
        r#"
    render = Noop
    /* HTTP_SHARE_POINT */
    app.del("/gone", fn() { return http.text("gone") })
    render = Other
"#,
        false,
    );
}

#[test]
fn http_shared_edges_second_service_does_not_erase_first_service_freeze() {
    rejects_after_sharing(
        "http-shared-two-services.ku",
        r#"
    render = Noop
    /* HTTP_SHARE_POINT */
    second = http.service()
    second.get("/other", fn() { return http.text("independent") })
    render = Other
"#,
        false,
    );
}

#[test]
fn http_shared_edges_disjoint_write_and_registration_branches_remain_legal() {
    accepts(
        "http-shared-disjoint-branches.ku",
        r#"
    render = Noop
    setter = () => { render = Other; return null }
    enabled = true
    if (enabled) {
        setter()
    } else {
        app.get("/", fn() { render(); return http.text("ok") })
    }
    render()
"#,
    );
}

#[test]
fn http_shared_edges_diverging_registration_does_not_freeze_other_path() {
    accepts(
        "http-shared-diverging-registration.ku",
        r#"
    render = Noop
    enabled = true
    if (enabled) {
        app.get("/", fn() { render(); return http.text("ok") })
        return ok(null)
    }
    render = Other
    render()
"#,
    );
}

#[test]
fn http_shared_edges_no_backedge_write_before_registration_remains_legal() {
    accepts(
        "http-shared-no-backedge.ku",
        r#"
    render = Noop
    while (true) {
        render = Other
        app.get("/", fn() { render(); return http.text("ok") })
        break
    }
    render()
"#,
    );
}

#[test]
fn http_shared_edges_frozen_callback_and_helper_calls_remain_legal() {
    accepts(
        "http-shared-normal-calls.ku",
        r#"
    render = Noop
    helper = () => { render(); return null }
    alias = helper.clone()
    app.get("/", fn() { alias(); return http.text("ok") })
    render()
    helper()
    alias()
    alias.clone()()
    Invoke(helper.clone())
    Invoke(render.clone())
"#,
    );
}

#[test]
fn http_shared_edges_two_services_can_share_and_call_the_same_cell() {
    accepts(
        "http-shared-two-services-read.ku",
        r#"
    render = Noop
    app.get("/", fn() { render(); return http.text("one") })
    second = http.service()
    second.get("/", fn() { render(); return http.text("two") })
    render()
"#,
    );
}

#[test]
fn http_shared_edges_known_setter_reports_assignment_and_registration_sites() {
    let source = program(
        r#"
    render = Noop
    setter = () => {
        render = Other
        return null
    }
    app.get("/", fn() { render(); return http.text("ok") })
    setter()
"#,
    );
    let error = check_raw("http-setter-sites.ku", &source).expect_err("shared write");
    assert_eq!(
        error.diagnostic_id(),
        DiagnosticId::HttpSharedCallableReassignment
    );
    let write_line = source
        .lines()
        .position(|line| line.contains("render = Other"))
        .unwrap()
        + 1;
    let register_line = source
        .lines()
        .position(|line| line.contains("app.get"))
        .unwrap()
        + 1;
    assert_eq!(error.span.start.line, write_line);
    assert!(error
        .message
        .contains(&format!("HTTP registration at line {register_line},")));
}

#[test]
fn http_shared_edges_template_registration_keeps_source_location() {
    let source = program(
        r#"
    render = Noop
    handler = () => { render(); return http.text("ok") }
    print(`注册 {app.get("/", handler.clone())}`)
    render = Other
"#,
    );
    let error = check_raw("http-template-registration-site.ku", &source).expect_err("shared write");
    assert_eq!(
        error.diagnostic_id(),
        DiagnosticId::HttpSharedCallableReassignment
    );
    let register_line = source
        .lines()
        .position(|line| line.contains("print(`"))
        .unwrap()
        + 1;
    let write_line = source
        .lines()
        .position(|line| line.contains("render = Other"))
        .unwrap()
        + 1;
    assert_eq!(error.span.start.line, write_line);
    assert!(
        error
            .message
            .contains(&format!("HTTP registration at line {register_line},")),
        "{error}"
    );
}

#[test]
fn http_shared_edges_malformed_template_reports_real_source_line() {
    let source = program("\n    text = `前文 {1 +}`\n    println(text)\n");
    let error =
        check_raw("http-template-syntax-site.ku", &source).expect_err("invalid interpolation");
    assert_eq!(error.kind, ku::error::KuErrorKind::Parse);
    let line = source
        .lines()
        .position(|line| line.contains("text ="))
        .unwrap()
        + 1;
    assert_eq!(error.span.start.line, line);
    assert!(error.span.start.column > 4);
}

#[test]
fn http_shared_edges_template_capture_token_budget_keeps_syntax_and_source_site() {
    let expression = "1 + ".repeat(600) + "1";
    let source = program(&format!(
        "\n    text = `前文 {{{expression}}}`\n    println(text)\n"
    ));
    let error = check_raw("http-template-budget-site.ku", &source)
        .expect_err("template capture token budget must reject before AST construction");
    assert_eq!(error.kind, ku::error::KuErrorKind::Parse);
    assert_eq!(
        error
            .diagnostic_data("http-template-budget-site.ku", &source)
            .code,
        "E0101"
    );
    assert!(error.message.contains("too many tokens"));
    assert!(error
        .message
        .contains("capture analysis limit: template token limit exceeded"));
    let template_line = source
        .lines()
        .position(|line| line.contains("text ="))
        .unwrap()
        + 1;
    assert_eq!(error.span.start.line, template_line);
    // This budget carries the whole template's real position. It must not be
    // rebased again as though it were an interpolation-relative parse error.
    assert_eq!(error.span.start.column, 12);
}
