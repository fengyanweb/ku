use ku::ast::{BinaryOp, ExprKind, Item, Stmt};
use ku::cli::{check_source, run_source};
use ku::lexer::Lexer;
use ku::parser::Parser;
use ku::token::TokenKind;

fn check_err(source: &str) -> String {
    check_source("inline.ku", source)
        .expect_err("program should fail")
        .to_string()
}

fn run_err(source: &str) -> String {
    run_source("inline.ku", source)
        .expect_err("program should fail")
        .to_string()
}

#[test]
fn lexer_tokenizes_core_literals_and_keywords() {
    let tokens = Lexer::new(r#"fn main() { print(`hi {1}`) null true false 12 3.5 "x" 'y' }"#)
        .tokenize()
        .expect("lex should pass");

    assert!(tokens
        .iter()
        .any(|token| matches!(token.kind, TokenKind::Fn)));
    assert!(tokens
        .iter()
        .any(|token| matches!(token.kind, TokenKind::TemplateString(_))));
    assert!(tokens
        .iter()
        .any(|token| matches!(token.kind, TokenKind::Null)));
    assert!(tokens
        .iter()
        .any(|token| matches!(token.kind, TokenKind::True)));
    assert!(tokens
        .iter()
        .any(|token| matches!(token.kind, TokenKind::Float(value) if value == 3.5)));
}

#[test]
fn parser_preserves_function_and_expression_shape() {
    let source = r#"
fn add(a: int, b: int): int {
    return a + b
}
"#;
    let tokens = Lexer::new(source).tokenize().expect("lex should pass");
    let program = Parser::new(tokens).parse().expect("parse should pass");

    let Item::Function(function) = &program.items[0] else {
        panic!("expected function item");
    };
    assert_eq!(function.name, "add");
    assert_eq!(function.params.len(), 2);
    match &function.body[0] {
        Stmt::Return {
            value: Some(expr), ..
        } => match &expr.kind {
            ExprKind::Binary { op, .. } => assert_eq!(*op, BinaryOp::Add),
            other => panic!("expected binary return, got {other:?}"),
        },
        other => panic!("expected return statement, got {other:?}"),
    }
}

#[test]
fn parser_expression_only_respects_precedence() {
    let tokens = Lexer::new("1 + 2 * 3").tokenize().expect("lex should pass");
    let expr = Parser::new(tokens)
        .parse_expression_only()
        .expect("parse expression should pass");

    match expr.kind {
        ExprKind::Binary {
            op: BinaryOp::Add,
            right,
            ..
        } => match right.kind {
            ExprKind::Binary {
                op: BinaryOp::Multiply,
                ..
            } => {}
            other => panic!("expected multiply on right side, got {other:?}"),
        },
        other => panic!("expected add expression, got {other:?}"),
    }
}

#[test]
fn interpreter_runs_builtin_string_functions() {
    let source = r##"
fn main() {
    print(len("Ku"))
    print(str(123) + "!")
}
"##;

    run_source("inline.ku", source).expect("builtins should run");
}

#[test]
fn local_function_value_can_shadow_builtin_name() {
    let source = r#"
fn main() {
    len = (x) => {
        return x + 1
    }
    total:int = len(41)
    print(total)
}
"#;

    check_source("inline.ku", source).expect("local function value should shadow builtin");
    run_source("inline.ku", source).expect("local function value should run");
}

#[test]
fn type_matrix_accepts_valid_core_operations() {
    let source = r##"
fn main() {
    i:int = 1 + 2
    f:float = 1 + 2.5
    b:bool = true && !false
    s:str = "Ku" + "!"
    n:null = null
    print(`{i} {f} {b} {s} {n} {1 + "x"}`)
}
"##;

    check_source("inline.ku", source).expect("valid type matrix should pass");
}

#[test]
fn type_matrix_rejects_invalid_core_operations() {
    for source in [
        r#"fn main() { print(true + 1) }"#,
        r#"fn main() { print("a" - "b") }"#,
        r#"fn main() { print(1 && 2) }"#,
        r#"fn main() { print("a" < "b") }"#,
        r#"fn main() { print(1.2 % 1.0) }"#,
    ] {
        let err = check_err(source);
        assert!(err.contains("type error"), "unexpected error: {err}");
    }
}

#[test]
fn null_equality_is_allowed_but_null_math_is_rejected() {
    check_source("inline.ku", "fn main() { print(null == null) }")
        .expect("null equality should pass");
    let err = check_err("fn main() { print(null + null) }");
    assert!(err.contains("type error"), "unexpected error: {err}");
}

#[test]
fn function_system_rejects_bad_signatures_and_calls() {
    for source in [
        r#"fn main(a: int) {}"#,
        r#"fn add(a: int, a: int): int { return a } fn main() { print(add(1, 2)) }"#,
        r#"fn add(a: int): int { return a } fn main() { print(add(1, 2)) }"#,
        r#"fn add(a: int): int { return a } fn main() { print(add("x")) }"#,
        r#"fn main() { x = 1; x() }"#,
        r#"fn main() { x = 1; f = () => { return x }; print(f()) }"#,
    ] {
        let err = check_err(source);
        assert!(
            err.contains("error:"),
            "expected diagnostic error, got: {err}"
        );
    }
}

#[test]
fn runtime_rejects_division_by_zero() {
    let err = run_err("fn main() { print(1 / 0) }");
    assert!(err.contains("division by zero"), "unexpected error: {err}");
}

#[test]
fn resource_limits_reject_deep_parse_and_huge_input() {
    let deep = format!(
        "fn main() {{ print({}1{}) }}",
        "(".repeat(64),
        ")".repeat(64)
    );
    let err = check_err(&deep);
    assert!(
        err.contains("maximum parse depth"),
        "unexpected deep parse error: {err}"
    );

    let huge = format!("fn main() {{ {} }}", "print(1) ".repeat(100_001));
    let err = check_err(&huge);
    assert!(
        err.contains("too many tokens"),
        "unexpected huge input error: {err}"
    );
}

#[test]
fn template_string_rejects_empty_and_unclosed_interpolation() {
    for source in [
        "fn main() { print(`bad {}`) }",
        "fn main() { print(`bad {1 + 2`) }",
    ] {
        let err = check_err(source);
        assert!(
            err.contains("template interpolation"),
            "unexpected template error: {err}"
        );
    }
}

#[test]
fn template_string_allows_escaped_braces() {
    let source = r#"
fn main() {
    print(`literal \{name\}`)
}
"#;

    check_source("inline.ku", source).expect("escaped braces should not be interpolation");
    run_source("inline.ku", source).expect("escaped braces should print");
}

#[test]
fn template_interpolation_obeys_token_limit() {
    let huge_expr = "1 + ".repeat(100_001) + "1";
    let source = format!("fn main() {{ print(`{{{huge_expr}}}`) }}");
    let err = check_err(&source);
    assert!(err.contains("too many tokens"), "unexpected error: {err}");
}

#[test]
fn lexer_rejects_bad_strings() {
    let err = Lexer::new("\"unterminated")
        .tokenize()
        .expect_err("unterminated string should fail")
        .to_string();
    assert!(err.contains("unterminated string"));

    let err = Lexer::new("\"bad\\q\"")
        .tokenize()
        .expect_err("bad escape should fail")
        .to_string();
    assert!(err.contains("unknown string escape"));
}

#[test]
fn string_escape_sequences_are_supported() {
    let tokens = Lexer::new(r#""a\nb" 'it\'s' "\\""#)
        .tokenize()
        .expect("escaped strings should lex");
    let strings: Vec<_> = tokens
        .iter()
        .filter_map(|token| match &token.kind {
            TokenKind::String(value) => Some(value.as_str()),
            _ => None,
        })
        .collect();

    assert_eq!(strings, ["a\nb", "it's", "\\"]);
}

#[test]
fn diagnostic_contains_file_line_column_and_source_line() {
    let err = check_err(
        r#"
fn main() {
    value:int = "bad"
}
"#,
    );

    assert!(err.contains("--> inline.ku:3:"), "missing location: {err}");
    assert!(
        err.contains("value:int = \"bad\""),
        "missing source line: {err}"
    );
    assert!(err.contains("^"), "missing caret: {err}");
}

#[test]
fn template_interpolation_type_errors_point_to_interpolation() {
    let err = check_err(
        r#"
fn main() {
    t1 = 1
    t2 = "2"
    print(`{t1-t2},t1={t1},t2={t2}`)
}
"#,
    );

    assert!(err.contains("--> inline.ku:5:"), "wrong location: {err}");
    assert!(err.contains("t1-t2"), "missing interpolation line: {err}");
    assert!(
        !err.starts_with("error: type error:"),
        "duplicate error prefix: {err}"
    );
}
