use ku::ast::{BinaryOp, ExprKind, Item, Stmt};
use ku::cli::{check_source, run_source};
use ku::lexer::Lexer;
use ku::parser::Parser;
use ku::span::{Position, Span};
use ku::token::{Token, TokenKind};

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
    let program = Parser::new(tokens)
        .parse_program()
        .expect("parse should pass");

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
    len = (x: int) => {
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
fn parser_expression_only_reports_eof_without_panicking() {
    let empty_tokens = Lexer::new("").tokenize().expect("empty source should lex");
    let empty_eof = empty_tokens.last().expect("lexer must emit EOF").span;
    let empty_error = Parser::new(empty_tokens)
        .parse_expression_only()
        .expect_err("empty expression must fail");
    assert_eq!(empty_error.message, "expected expression");
    assert_eq!(empty_error.span, empty_eof);

    let explicit_eof = Span::point(Position::new(7, 3, 42));
    let explicit_error = Parser::new(vec![Token::new(TokenKind::Eof, explicit_eof)])
        .parse_expression_only()
        .expect_err("an EOF-only token stream must fail");
    assert_eq!(explicit_error.message, "expected expression");
    assert_eq!(explicit_error.span, explicit_eof);

    let trailing_operator_tokens = Lexer::new("1 +")
        .tokenize()
        .expect("trailing operator fixture should lex");
    let trailing_eof = trailing_operator_tokens
        .last()
        .expect("lexer must emit EOF")
        .span;
    let trailing_error = Parser::new(trailing_operator_tokens)
        .parse_expression_only()
        .expect_err("operator followed by EOF must fail");
    assert_eq!(trailing_error.message, "expected expression");
    assert_eq!(trailing_error.span, trailing_eof);
}

#[test]
fn parser_type_atom_reports_eof_and_boundary_spans_without_panicking() {
    for source in ["fn f(value: int |", "fn f(value: fn(int |"] {
        let tokens = Lexer::new(source)
            .tokenize()
            .expect("incomplete type fixture should lex");
        let eof_span = tokens.last().expect("lexer must emit EOF").span;
        let error = Parser::new(tokens)
            .parse_program()
            .expect_err("a union arm missing at EOF must fail");
        assert_eq!(error.message, "expected type name");
        assert_eq!(error.span, eof_span);
    }

    let boundary_source = "fn f(value: int |) {}";
    let boundary_tokens = Lexer::new(boundary_source)
        .tokenize()
        .expect("bounded incomplete type fixture should lex");
    let boundary_span = boundary_tokens
        .iter()
        .find(|token| matches!(token.kind, TokenKind::RParen))
        .expect("fixture must contain a parameter boundary")
        .span;
    let boundary_error = Parser::new(boundary_tokens)
        .parse_program()
        .expect_err("a union arm missing before ')' must fail");
    assert_eq!(boundary_error.message, "expected type name");
    assert_eq!(boundary_error.span, boundary_span);
}

#[test]
fn parser_bounds_structural_type_depth_across_all_type_forms() {
    let parse = |source: &str| {
        let tokens = Lexer::new(source).tokenize().expect("fixture should lex");
        Parser::new(tokens).parse_program()
    };
    let assert_depth_error = |source: &str| {
        let error = parse(source).expect_err("type nesting above the limit must fail");
        assert_eq!(
            error.message,
            "maximum type depth exceeded; type is too deeply nested"
        );
    };

    let array_at_limit = format!("fn f(value: {}int{}) {{}}", "[".repeat(32), "]".repeat(32));
    parse(&array_at_limit).expect("32 nested array types must remain valid");
    let array_over_limit = format!("fn f(value: {}int{}) {{}}", "[".repeat(33), "]".repeat(33));
    assert_depth_error(&array_over_limit);

    let result_at_limit = format!("fn f(value: {}int!{}) {{}}", "[".repeat(31), "]".repeat(31));
    parse(&result_at_limit).expect("31 arrays around one result must have depth 32");
    let result_over_limit = format!("fn f(value: {}int!{}) {{}}", "[".repeat(32), "]".repeat(32));
    assert_depth_error(&result_over_limit);

    let nested_function_type = |depth: usize| {
        let mut ty = "int".to_string();
        for _ in 0..depth {
            ty = format!("fn(): {ty}");
        }
        ty
    };
    parse(&format!("fn f(value: {}) {{}}", nested_function_type(32)))
        .expect("32 nested function types must remain valid");
    assert_depth_error(&format!("fn f(value: {}) {{}}", nested_function_type(33)));

    let branch = format!("{}int{}", "[".repeat(31), "]".repeat(31));
    parse(&format!(
        "fn f(value: fn({branch}, {branch}): {branch}) {{}}"
    ))
    .expect("function type sibling branches must not accumulate parse depth");

    let mixed_at_limit = format!(
        "fn f(value: {}fn(): int! | str{} | bool) {{}}",
        "[".repeat(30),
        "]".repeat(30)
    );
    parse(&mixed_at_limit)
        .expect("array, result, function, and union nesting at depth 32 must parse");
    let mixed_over_limit = format!(
        "fn f(value: {}fn(): int! | str{}! | bool) {{}}",
        "[".repeat(30),
        "]".repeat(30)
    );
    assert_depth_error(&mixed_over_limit);

    let arrow_at_limit = format!(
        "fn main() {{ op = (value: {}int{}) => value }}",
        "[".repeat(32),
        "]".repeat(32)
    );
    parse(&arrow_at_limit).expect("arrow lookahead must accept the type-depth boundary");
    let arrow_over_limit = format!(
        "fn main() {{ op = (value: {}int{}) => value }}",
        "[".repeat(33),
        "]".repeat(33)
    );
    assert_depth_error(&arrow_over_limit);

    let arrow_return_at_limit = format!(
        "fn main() {{ op = (): {}int{} => 0 }}",
        "[".repeat(32),
        "]".repeat(32)
    );
    parse(&arrow_return_at_limit)
        .expect("arrow lookahead must accept the return-type depth boundary");
    let arrow_return_over_limit = format!(
        "fn main() {{ op = (): {}int{} => 0 }}",
        "[".repeat(33),
        "]".repeat(33)
    );
    assert_depth_error(&arrow_return_over_limit);

    let incomplete = format!("fn f(value: {}int |", "[".repeat(32));
    let tokens = Lexer::new(&incomplete)
        .tokenize()
        .expect("incomplete type fixture should lex");
    let eof_span = tokens.last().expect("lexer must emit EOF").span;
    let error = Parser::new(tokens)
        .parse_program()
        .expect_err("EOF inside a depth-limited type must fail cleanly");
    assert_eq!(error.message, "expected type name");
    assert_eq!(error.span, eof_span);
}

#[test]
fn parser_rejects_invalid_token_streams_without_panicking() {
    let program_error = Parser::new(Vec::new())
        .parse_program()
        .expect_err("empty program token stream must fail");
    assert_eq!(program_error.message, "token stream is empty");
    assert_eq!(program_error.span, Span::default());

    let expression_error = Parser::new(Vec::new())
        .parse_expression_only()
        .expect_err("empty expression token stream must fail");
    assert_eq!(expression_error.message, "token stream is empty");
    assert_eq!(expression_error.span, Span::default());

    let value_span = Span::point(Position::new(2, 4, 9));
    let missing_program_eof = Parser::new(vec![Token::new(TokenKind::Int(1), value_span)])
        .parse_program()
        .expect_err("program token stream missing EOF must fail closed");
    assert_eq!(missing_program_eof.message, "token stream is missing EOF");
    assert_eq!(missing_program_eof.span, value_span);

    let missing_expression_eof = Parser::new(vec![Token::new(TokenKind::Int(1), value_span)])
        .parse_expression_only()
        .expect_err("expression token stream missing EOF must fail closed");
    assert_eq!(
        missing_expression_eof.message,
        "token stream is missing EOF"
    );
    assert_eq!(missing_expression_eof.span, value_span);

    let eof_span = Span::point(Position::new(1, 1, 0));
    let early_eof = Parser::new(vec![
        Token::new(TokenKind::Eof, eof_span),
        Token::new(TokenKind::Int(1), value_span),
    ])
    .parse_program()
    .expect_err("non-final EOF must fail closed");
    assert_eq!(early_eof.message, "EOF must be the final token");
    assert_eq!(early_eof.span, eof_span);

    let final_eof_span = Span::point(Position::new(2, 5, 10));
    let early_eof_with_final_eof = Parser::new(vec![
        Token::new(TokenKind::Eof, eof_span),
        Token::new(TokenKind::Int(1), value_span),
        Token::new(TokenKind::Eof, final_eof_span),
    ])
    .parse_expression_only()
    .expect_err("an EOF before the final EOF must fail closed");
    assert_eq!(
        early_eof_with_final_eof.message,
        "EOF must be the final token"
    );
    assert_eq!(early_eof_with_final_eof.span, eof_span);

    let oversized_span = Span::point(Position::new(3, 2, 11));
    let oversized_broken_stream = vec![Token::new(TokenKind::Int(1), oversized_span); 100_001];
    let oversized_error = Parser::new(oversized_broken_stream)
        .parse_expression_only()
        .expect_err("token limit must take priority over malformed stream shape");
    assert_eq!(
        oversized_error.message,
        "too many tokens; input is too large for Ku"
    );
    assert_eq!(oversized_error.span, oversized_span);
}

#[test]
fn interpreter_integer_comparisons_preserve_i64_precision() {
    let source = r#"
fn assert_ordered(lower: int, higher: int) {
    if (!(lower < higher)) panic("less")
    if (!(lower <= higher)) panic("less equal ordered")
    if (lower > higher) panic("greater reversed")
    if (lower >= higher) panic("greater equal reversed")
    if (lower == higher) panic("equal distinct")
    if (!(lower != higher)) panic("not equal distinct")

    if (higher < lower) panic("less reversed")
    if (higher <= lower) panic("less equal reversed")
    if (!(higher > lower)) panic("greater")
    if (!(higher >= lower)) panic("greater equal ordered")

    if (lower < lower) panic("less equal values")
    if (!(lower <= lower)) panic("less equal equal values")
    if (lower > lower) panic("greater equal values")
    if (!(lower >= lower)) panic("greater equal equal values")
    if (!(lower == lower)) panic("equal same")
    if (lower != lower) panic("not equal same")
}

fn main() {
    minimum = -9223372036854775807 - 1
    assert_ordered(minimum, -9223372036854775807)
    assert_ordered(9223372036854775806, 9223372036854775807)
}
"#;

    check_source("inline.ku", source).expect("i64 boundary comparisons should check");
    run_source("inline.ku", source).expect("i64 boundary comparisons should be exact");
}

#[test]
fn function_system_rejects_bad_signatures_and_calls() {
    for source in [
        r#"fn main(a: int) {}"#,
        r#"fn add(a: int, a: int): int { return a } fn main() { print(add(1, 2)) }"#,
        r#"fn add(a: int): int { return a } fn main() { print(add(1, 2)) }"#,
        r#"fn add(a: int): int { return a } fn main() { print(add("x")) }"#,
        r#"fn main() { x = 1; x() }"#,
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
