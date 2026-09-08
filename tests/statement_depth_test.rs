use ku::{
    ast::{Item, Program, Stmt},
    checker::Checker,
    error::{DiagnosticId, KuError, KuResult},
    lexer::Lexer,
    parser::Parser,
    token::TokenKind,
};

#[allow(dead_code)]
#[path = "support/native_pg_harness.rs"]
mod native_harness;

fn parse(source: &str) -> KuResult<Program> {
    Parser::new(Lexer::new(source).lex()?).parse_program()
}

fn nest(open: &str, close: &str, count: usize, leaf: &str) -> String {
    format!("{}{}{}", open.repeat(count), leaf, close.repeat(count))
}

fn in_main(body: &str) -> String {
    format!("fn main() {{ {body} }}")
}

fn assert_statement_error(error: KuError) {
    assert_eq!(error.diagnostic_id(), DiagnosticId::SyntaxError);
    assert_eq!(
        error.message,
        "maximum parse depth exceeded; statement body is too deeply nested"
    );
}

#[test]
fn statement_depth_parser_braced_single_and_mixed_control_boundaries() {
    for (open, close) in [
        ("if (true) {", "}"),
        ("while (true) {", "}"),
        ("for n in [1] {", "}"),
        ("try {", "} catch(err) {}"),
        ("if (true) ", ""),
        ("while (true) ", ""),
        ("for n in [1] ", ""),
    ] {
        parse(&in_main(&nest(open, close, 32, "print(1)"))).unwrap();
        assert_statement_error(parse(&in_main(&nest(open, close, 33, "print(1)"))).unwrap_err());
    }
    for depth in [32, 33] {
        let body = (0..depth)
            .map(|i| {
                if i % 2 == 0 {
                    "if (true) {"
                } else {
                    "while (true) {"
                }
            })
            .collect::<String>();
        let source = in_main(&format!("{body} print(1) {}", "}".repeat(depth)));
        if depth == 32 {
            let program = parse(&source).unwrap();
            Checker::new().check(&program).unwrap();
        } else {
            assert_statement_error(parse(&source).unwrap_err());
        }
    }
}

#[test]
fn statement_depth_parser_else_if_uses_the_33rd_keyword_and_preserves_shape() {
    for depth in [32, 33] {
        let source = in_main(&format!(
            "if (true) {{}} {}",
            "else if (true) {} ".repeat(depth - 1)
        ));
        let tokens = Lexer::new(&source).lex().unwrap();
        if depth == 33 {
            let span = tokens
                .iter()
                .filter(|t| t.kind == TokenKind::If)
                .nth(32)
                .unwrap()
                .span;
            let error = Parser::new(tokens).parse_program().unwrap_err();
            assert_eq!(error.span, span);
            assert_statement_error(error);
        } else {
            let program = Parser::new(tokens).parse_program().unwrap();
            let Item::Function(function) = &program.items[0] else {
                panic!("function")
            };
            let mut branch = &function.body;
            for _ in 0..32 {
                let Stmt::If { else_branch, .. } = &branch[0] else {
                    panic!("if")
                };
                branch = else_branch;
            }
            assert!(branch.is_empty());
        }
    }
    let source = "fn main() { if (true) if (false) print(1); else print(2); print(3) }";
    let program = parse(source).unwrap();
    let Item::Function(function) = &program.items[0] else {
        panic!("function")
    };
    assert_eq!(function.body.len(), 2);
    let Stmt::If {
        then_branch,
        else_branch,
        ..
    } = &function.body[0]
    else {
        panic!("if")
    };
    assert!(else_branch.is_empty());
    let Stmt::If { else_branch, .. } = &then_branch[0] else {
        panic!("nested if")
    };
    assert_eq!(else_branch.len(), 1);
}

#[test]
fn statement_depth_parser_siblings_and_top_level_roots_are_independent() {
    let arm = nest("if (true) {", "}", 31, "print(1)");
    let body = format!("if (true) {{ {arm} }} else {{ {arm} }}");
    parse(&format!(
        "fn main() {{ {body} {body} }} fn other() {{ {body} }}"
    ))
    .unwrap();
    parse(&in_main(&"if (true) {} ".repeat(128))).unwrap();
    let arm = nest("try {", "} catch(err) {}", 31, "print(1)");
    parse(&in_main(&format!(
        "try {{ {arm} }} catch(err) {{ {arm} }} finally {{ {arm} }}"
    )))
    .unwrap();
}

#[test]
fn statement_depth_parser_local_functions_and_value_bodies_do_not_reset() {
    for open in ["fn local() {", "async fn local(): null! {"] {
        parse(&in_main(&nest(open, "}", 32, ""))).unwrap();
        assert_statement_error(parse(&in_main(&nest(open, "}", 33, ""))).unwrap_err());
    }
    for value in ["fn() {}", "() => {}", "x => {}", "x: int => {}"] {
        let leaf = format!("value = {value}");
        parse(&in_main(&nest("if (true) {", "}", 31, &leaf))).unwrap();
        assert_statement_error(parse(&in_main(&nest("if (true) {", "}", 32, &leaf))).unwrap_err());
        let condition = format!("if ({value}) {{}}");
        parse(&in_main(&nest("if (true) {", "}", 30, &condition))).unwrap();
        assert_statement_error(
            parse(&in_main(&nest("if (true) {", "}", 31, &condition))).unwrap_err(),
        );
    }
}

#[test]
fn statement_depth_parser_expression_only_retains_body_and_expression_limits() {
    for (open, leaf) in [
        ("fn() { value = ", "fn() {}"),
        ("() => { value = ", "() => {}"),
        ("x => { value = ", "x => {}"),
        ("x: int => { value = ", "x: int => {}"),
    ] {
        let source = nest(open, "}", 31, leaf);
        Parser::new(Lexer::new(&source).lex().unwrap())
            .parse_expression_only()
            .unwrap();
    }
    let source = format!("() => {{ {} }}", nest("if (true) {", "}", 32, "print(1)"));
    assert_statement_error(
        Parser::new(Lexer::new(&source).lex().unwrap())
            .parse_expression_only()
            .unwrap_err(),
    );
    let source = in_main(&format!("print({}1{})", "(".repeat(64), ")".repeat(64)));
    let error = parse(&source).unwrap_err();
    assert_eq!(
        error.message,
        "maximum parse depth exceeded; expression is too deeply nested"
    );
    let source = in_main(&format!("value: {}int{}", "[".repeat(33), "]".repeat(33)));
    let error = parse(&source).unwrap_err();
    assert_eq!(
        error.message,
        "maximum type depth exceeded; type is too deeply nested"
    );
}

#[test]
fn statement_depth_parser_32_controls_match_existing_stage3_positive_inputs() {
    for kind in ["if", "while"] {
        let source = format!(
            "fn main() {{{}value = 1{}}}",
            format!("{kind} (true) {{").repeat(32),
            "}".repeat(32)
        );
        let program = parse(&source).unwrap();
        Checker::new().check(&program).unwrap();
        let rejected = format!(
            "fn main() {{{}value = 1{}}}",
            format!("{kind} (true) {{").repeat(33),
            "}".repeat(33)
        );
        let tokens = Lexer::new(&rejected).lex().unwrap();
        let keyword = if kind == "if" {
            TokenKind::If
        } else {
            TokenKind::While
        };
        let span = tokens
            .iter()
            .filter(|t| t.kind == keyword)
            .nth(32)
            .unwrap()
            .span;
        let error = Parser::new(tokens).parse_program().unwrap_err();
        assert_eq!(error.span, span);
        assert_statement_error(error);
    }
}

#[test]
fn statement_depth_nested_loop_analysis_cli_is_bounded() {
    use native_harness::{run_bounded, TempDir, RUN_LIMITS, RUN_TIMEOUT};
    use std::{fs, process::Command};

    let directory = TempDir::new("statement-depth-loop-budget");
    let path = directory.path().join("program.ku");
    let source = in_main(&format!(
        "value = 1 {} print(value) {}",
        "while (true) {".repeat(32),
        "}".repeat(32)
    ));
    fs::write(&path, source).unwrap();
    let output = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_ku"))
            .arg("check")
            .arg(&path),
        RUN_TIMEOUT,
        RUN_LIMITS,
    )
    .expect("nested loop checking must terminate within the existing process bound");
    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(
        error.contains("maximum check work exceeded; loop ownership analysis budget exhausted"),
        "{error}"
    );
}
