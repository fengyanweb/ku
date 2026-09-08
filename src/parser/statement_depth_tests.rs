use super::*;
use crate::lexer::Lexer;

#[test]
fn statement_depth_guard_restores_on_errors_and_checks_before_entering() {
    let mut parser = Parser::new(Lexer::new("").lex().unwrap());
    let span = Span::default();
    let result: KuResult<()> =
        parser.with_statement_depth(span, |_| Err(KuError::parse("ordinary parse error", span)));
    assert!(result.is_err());
    assert_eq!(parser.statement_depth, 0);
    parser.statement_depth = MAX_STATEMENT_DEPTH;
    let mut called = false;
    let result = parser.with_statement_depth(span, |_| {
        called = true;
        Ok(())
    });
    assert!(result.is_err());
    assert!(!called);
    assert_eq!(parser.statement_depth, MAX_STATEMENT_DEPTH);
}

#[test]
fn statement_depth_all_parser_owner_errors_release_the_counter() {
    for source in [
        "fn main() { if (true) { if (true) } }",
        "fn main() { if (true) {} else if (true) }",
        "fn main() { while (true) { if (true) } }",
        "fn main() { for n in [1] { if (true) } }",
        "fn main() { try {} catch(err) { if (true) } }",
        "fn main() { try {} finally { if (true) } }",
        "fn main() { fn nested() { if (true) } }",
        "fn main() { async fn nested(): null! { if (true) } }",
        "fn main() { value = fn() { if (true) } }",
        "fn main() { value = () => { if (true) } }",
        "fn main() { value = x => { if (true) } }",
        "fn main() { value = x: int => { if (true) } }",
    ] {
        let mut parser = Parser::new(Lexer::new(source).lex().unwrap());
        assert!(parser.parse_program().is_err(), "{source}");
        assert_eq!(parser.statement_depth, 0, "{source}");
    }
}
