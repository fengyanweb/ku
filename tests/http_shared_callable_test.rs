//! Checker-only contracts for freezing HTTP-shared callable BindingIds.
//! No source is run: these fixtures never start a server, compiler or database.
//! Parse each fixture separately so a syntax failure cannot satisfy a rejection.

use ku::{cli::check_source, lexer::Lexer, parser::Parser};

fn program(body: &str) -> String {
    format!(
        r#"
import "std.http"
fn Noop(): null {{ return null }}
fn Other(): null {{ return null }}
fn main(): null! {{
    app = http.service({{ max_active_requests: 2, max_connections: 4 }})
    {body}
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

fn accepts(name: &str, body: &str) {
    let source = program(body);
    parses(name, &source);
    check_source(name, &source)
        .unwrap_or_else(|error| panic!("{name}: expected legal source: {}", error.message));
}

fn rejects_reassignment(name: &str, body: &str, binding: &str) {
    let source = program(body);
    parses(name, &source);
    let error = check_source(name, &source)
        .expect_err("HTTP-shared callable reassignment must be rejected");
    assert!(
        error.message.contains("E0704")
            && error
                .message
                .contains("cannot reassign HTTP-shared function binding")
            && error.message.contains(&format!("'{binding}'")),
        "{name}: expected E0704 for {binding:?}, not an unrelated rejection: {}",
        error.message
    );
}

#[test]
fn http_shared_callable_rejects_direct_captured_cell_replacement() {
    rejects_reassignment(
        "http-shared-direct.ku",
        r#"
    count = 0
    render = Noop
    app.get("/", fn() { render(); return http.text("ok") })
    render = () => { count += 1; return null }
"#,
        "render",
    );
}

#[test]
fn http_shared_callable_rejects_even_readonly_same_signature_replacement() {
    // Freezing is an identity/write contract, not a best-effort purity proof.
    rejects_reassignment(
        "http-shared-readonly-replacement.ku",
        r#"
    render = Noop
    app.get("/", fn() { render(); return http.text("ok") })
    render = Other
"#,
        "render",
    );
}

#[test]
fn http_shared_callable_rejects_multihop_captured_cell_replacement() {
    rejects_reassignment(
        "http-shared-multihop.ku",
        r#"
    render = Noop
    middle = () => { render(); return null }
    outer = () => { middle(); return null }
    app.get("/", fn() { outer(); return http.text("ok") })
    render = Other
"#,
        "render",
    );
}

#[test]
fn http_shared_callable_rejects_predeclared_named_setter_call() {
    rejects_reassignment(
        "http-shared-named-setter.ku",
        r#"
    render = Noop
    fn Set(): null { render = Other; return null }
    app.get("/", fn() { render(); return http.text("ok") })
    Set()
"#,
        "render",
    );
}

#[test]
fn http_shared_callable_rejects_predeclared_closure_setter_call() {
    rejects_reassignment(
        "http-shared-closure-setter.ku",
        r#"
    render = Noop
    setter = () => { render = Other; return null }
    app.get("/", fn() { render(); return http.text("ok") })
    setter()
"#,
        "render",
    );
}

#[test]
fn http_shared_callable_rejects_cloned_setter_alias_call() {
    rejects_reassignment(
        "http-shared-cloned-setter.ku",
        r#"
    render = Noop
    setter = () => { render = Other; return null }
    alias = setter.clone()
    second = alias.clone()
    app.get("/", fn() { render(); return http.text("ok") })
    second()
"#,
        "render",
    );
}

#[test]
fn http_shared_callable_rejects_moved_setter_alias_call() {
    rejects_reassignment(
        "http-shared-moved-setter.ku",
        r#"
    render = Noop
    setter = () => { render = Other; return null }
    alias = setter
    app.get("/", fn() { render(); return http.text("ok") })
    alias()
"#,
        "render",
    );
}

#[test]
fn http_shared_callable_keeps_freeze_after_registration_branch_join() {
    rejects_reassignment(
        "http-shared-registration-join.ku",
        r#"
    render = Noop
    enabled = true
    if (enabled) {
        app.get("/", fn() { render(); return http.text("ok") })
    }
    render = Other
"#,
        "render",
    );
}

#[test]
fn http_shared_callable_rejects_replacement_inside_a_branch() {
    rejects_reassignment(
        "http-shared-replacement-branch.ku",
        r#"
    render = Noop
    enabled = true
    app.get("/", fn() { render(); return http.text("ok") })
    if (enabled) { render = Other }
    else { render = Noop }
"#,
        "render",
    );
}

#[test]
fn http_shared_callable_keeps_freeze_across_loop_backedge() {
    // The first write is before registration, but the same statement on the
    // next iteration writes the cell already retained by the route.
    rejects_reassignment(
        "http-shared-loop-backedge.ku",
        r#"
    render = Noop
    turn = 0
    while (turn < 2) {
        render = Other
        if (turn == 0) {
            app.get("/", fn() { render(); return http.text("ok") })
        }
        turn += 1
    }
"#,
        "render",
    );
}

#[test]
fn http_shared_callable_keeps_freeze_across_continue_backedge() {
    rejects_reassignment(
        "http-shared-continue-backedge.ku",
        r#"
    render = Noop
    turn = 0
    while (turn < 2) {
        render = Other
        turn += 1
        if (turn == 1) {
            app.get("/", fn() { render(); return http.text("ok") })
            continue
        }
    }
"#,
        "render",
    );
}

#[test]
fn http_shared_callable_rejects_destructuring_write_to_shared_cell() {
    rejects_reassignment(
        "http-shared-destructure.ku",
        r#"
    render = Noop
    app.get("/", fn() { render(); return http.text("ok") })
    render, _ = Other, 0
"#,
        "render",
    );
}

#[test]
fn http_shared_callable_allows_unshared_reassignment() {
    accepts(
        "http-unshared-reassignment.ku",
        r#"
    render = Noop
    render = Other
    render()
    app.get("/", fn() { return http.text("ok") })
    render = Noop
    render()
"#,
    );
}

#[test]
fn http_shared_callable_allows_setter_before_registration() {
    accepts(
        "http-setter-before-registration.ku",
        r#"
    render = Noop
    setter = () => { render = Other; return null }
    setter()
    app.get("/", fn() { render(); return http.text("ok") })
"#,
    );
}

#[test]
fn http_shared_callable_allows_predeclared_setter_that_is_not_called() {
    accepts(
        "http-setter-not-called.ku",
        r#"
    render = Noop
    setter = () => { render = Other; return null }
    app.get("/", fn() { render(); return http.text("ok") })
"#,
    );
}

#[test]
fn http_shared_callable_direct_route_value_does_not_freeze_its_variable() {
    accepts(
        "http-direct-handler-value.ku",
        r#"
    handler = () => { return http.text("registered") }
    app.get("/", handler)
    handler = () => { return http.text("not registered") }
"#,
    );
}

#[test]
fn http_shared_callable_cloned_value_is_not_the_original_variable_cell() {
    accepts(
        "http-cloned-value-not-cell.ku",
        r#"
    render = Noop
    old = render.clone()
    app.get("/", fn() { old(); return http.text("ok") })
    render = Other
"#,
    );
}

#[test]
fn http_shared_callable_allows_explicit_same_name_local_binding() {
    // An annotated declaration creates a different BindingId, unlike assignment.
    accepts(
        "http-explicit-local-shadow.ku",
        r#"
    render = Noop
    app.get("/", fn() { render(); return http.text("ok") })
    if (true) {
        render: fn(): null = Noop
        render = Other
        render()
    }
    render()
"#,
    );
}

#[test]
fn http_shared_callable_request_parameter_does_not_capture_outer_homonym() {
    // Ordinary HTTP request parameters must be named req, not arbitrary names.
    accepts(
        "http-request-parameter-shadow.ku",
        r#"
    req = Noop
    app.get("/user/{id}", fn(req) { return http.text(req.path.clone()) })
    req = Other
    req()
"#,
    );
}

#[test]
fn http_shared_callable_allows_copy_capture_initialization_after_registration() {
    accepts(
        "http-copy-capture-initialization.ku",
        r#"
    count = 0
    render = () => { println(count); return null }
    app.get("/", fn() { render(); return http.text("ok") })
    copy = count
    println(copy)
"#,
    );
}

#[test]
fn http_shared_callable_allows_handler_local_callable_replacement() {
    accepts(
        "http-handler-local-callable.ku",
        r#"
    app.get("/", fn() {
        count = 0
        render = Noop
        render = () => { count += 1; return null }
        render()
        return http.text("ok")
    })
"#,
    );
}

const REGISTRATION_POINT: &str = "/* HTTP_REGISTRATION_POINT */";

fn rejects_registration_pair(name: &str, source: &str, registration: &str, expected_error: &str) {
    assert_eq!(source.matches(REGISTRATION_POINT).count(), 1);
    // Keep all assignments, aliases and calls: only route registration differs.
    // A syntax/type/ownership error must fail this positive control first.
    let control_name = format!("control-{name}");
    let control = source.replace(REGISTRATION_POINT, "");
    parses(&control_name, &control);
    check_source(&control_name, &control).unwrap_or_else(|error| {
        panic!(
            "{control_name}: legal control rejected: {}\n{control}",
            error.message
        )
    });

    let registered = source.replace(REGISTRATION_POINT, registration);
    parses(name, &registered);
    let error = check_source(name, &registered)
        .expect_err("registration must reject the shared mutation or missing safety proof");
    let explicit_proof_failure = error.message.starts_with("error[E0704]:")
        && error.message.contains("cannot prove")
        && error.message.contains("HTTP-shared");
    let (code, message) = expected_error.split_once(": ").expect("code and detail");
    assert!(
        (error.message.starts_with(code) && error.message.contains(message))
            || explicit_proof_failure,
        "{name}: expected the relevant HTTP safety diagnostic, not another error: {}\n{registered}",
        error.message
    );
}

#[test]
fn http_shared_callable_installed_mutator_clone_cannot_reuse_stale_body() {
    rejects_registration_pair(
        "http-installed-handler-clone.ku",
        &program(
            r#"
    count = 0
    handler = () => { return http.text("initial") }
    Set = () => {
        handler = () => { count += 1; return http.text("mutated") }
        return null
    }
    handler = () => { return http.text("initial") }
    Set()
    /* HTTP_REGISTRATION_POINT */
"#,
        ),
        "app.get(\"/\", handler.clone())",
        "error[E0703]: http handler cannot modify captured variable 'count'",
    );
}

#[test]
fn http_shared_callable_installed_mutator_new_alias_cannot_reuse_stale_body() {
    rejects_registration_pair(
        "http-installed-handler-alias.ku",
        &program(
            r#"
    count = 0
    handler = () => { return http.text("initial") }
    Set = () => {
        handler = () => { count += 1; return http.text("mutated") }
        return null
    }
    handler = () => { return http.text("initial") }
    Set()
    newalias = handler.clone()
    /* HTTP_REGISTRATION_POINT */
"#,
        ),
        "app.get(\"/\", newalias)",
        "error[E0703]: http handler cannot modify captured variable 'count'",
    );
}

#[test]
fn http_shared_callable_template_only_setter_call_keeps_write_effect() {
    // The program parser does not parse template interpolation ASTs itself.
    // Validate the real hidden call syntax separately as well as the control.
    let tokens = Lexer::new("setter()")
        .lex()
        .expect("template setter call must lex");
    Parser::new(tokens)
        .parse_expression_only()
        .expect("template setter call must parse");
    rejects_registration_pair(
        "http-template-only-setter.ku",
        &program(
            r#"
    render = Noop
    setter = () => { render = Other; return 0 }
    hidden = () => { print(`{setter()}`); return null }
    /* HTTP_REGISTRATION_POINT */
    hidden()
"#,
        ),
        "app.get(\"/\", fn() { render(); return http.text(\"ok\") })",
        "error[E0704]: cannot reassign HTTP-shared function binding 'render'",
    );
}

#[test]
fn http_shared_callable_local_choose_return_cannot_use_top_level_provenance() {
    rejects_registration_pair(
        "http-shadowed-top-level-return.ku",
        r#"
import "std.http"
fn Noop(): null { return null }
fn Other(): null { return null }
fn Choose(): fn(): null { return fn(): null { return null } }
fn main(): null! {
    app = http.service()
    render = Noop
    Set = () => { render = Other; return null }
    Choose: fn(): fn(): null = fn(): fn(): null { return Set.clone() }
    action = Choose()
    /* HTTP_REGISTRATION_POINT */
    action()
    return ok(null)
}
"#,
        "app.get(\"/\", fn() { render(); return http.text(\"ok\") })",
        "error[E0704]: cannot reassign HTTP-shared function binding 'render'",
    );
}
