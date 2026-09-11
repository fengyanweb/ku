use ku::{lexer::Lexer, parser::Parser, token::TokenKind};

#[test]
fn parser_missing_names_and_import_strings_report_the_actual_eof() {
    for (source, expected) in [
        ("fn", "expected function name"),
        ("fn f(", "expected parameter name"),
        ("fn f(&", "expected parameter name"),
        ("fn f(x,", "expected parameter name"),
        ("fn f<T,", "expected generic type parameter"),
        ("struct", "expected struct name"),
        ("enum", "expected enum name"),
        ("import {", "expected imported name"),
        ("import { Thing as", "expected import alias"),
        ("import name from", "expected import path string"),
        ("fn main() { value.", "expected field name after '.'"),
    ] {
        let tokens = Lexer::new(source).lex().unwrap();
        let eof = tokens.last().unwrap();
        assert!(matches!(eof.kind, TokenKind::Eof));
        let span = eof.span;
        let error = Parser::new(tokens).parse_program().unwrap_err();
        assert_eq!(error.span, span, "{source:?}: {error}");
        assert_eq!(error.message, expected, "{source:?}: {error}");
    }
}

#[test]
fn parser_eof_does_not_reuse_a_preceding_literal_or_underflow() {
    assert!(Parser::new(Lexer::new("").lex().unwrap())
        .parse_program()
        .unwrap()
        .items
        .is_empty());
    let empty = Lexer::new("").lex().unwrap();
    let eof = empty.last().unwrap().span;
    assert_eq!(
        Parser::new(empty).parse_expression_only().unwrap_err().span,
        eof
    );
    Parser::new(Lexer::new("\"done\"").lex().unwrap())
        .parse_expression_only()
        .unwrap();
    let source = "import \"first\"\nimport name from";
    let tokens = Lexer::new(source).lex().unwrap();
    let eof = tokens.last().unwrap().span;
    let error = Parser::new(tokens).parse_program().unwrap_err();
    assert_eq!(error.span, eof);
}
