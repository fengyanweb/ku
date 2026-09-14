use super::*;
use crate::{lexer::Lexer, parser::Parser, span::Position};

fn parse(source: &str) -> Program {
    Parser::new(Lexer::new(source).lex().unwrap())
        .parse_program()
        .unwrap()
}

#[test]
fn loop_analysis_budget_is_exact_sticky_and_keeps_first_span() {
    let mut checker = Checker::new();
    checker.loop_analysis_remaining = 2;
    let first = Span::point(Position::new(1, 1, 0));
    let later = Span::point(Position::new(2, 1, 10));
    checker.charge_loop_analysis(first).unwrap();
    checker.charge_loop_analysis(first).unwrap();
    assert_eq!(checker.loop_analysis_remaining, 0);
    checker.check_loop_analysis_budget().unwrap();
    let error = checker.charge_loop_analysis(first).unwrap_err();
    assert_eq!(error.span, first);
    assert_eq!(checker.charge_loop_analysis(later).unwrap_err(), error);
    assert_eq!(checker.loop_analysis_remaining, 0);
}

#[test]
fn loop_analysis_empty_outer_bindings_skip_only_speculation() {
    let source = format!(
        "fn main() {{ {} value = 1 {} }}",
        "while (true) {".repeat(32),
        "}".repeat(32)
    );
    let mut checker = Checker::new();
    checker.loop_analysis_remaining = 0;
    checker.check_program(&parse(&source)).unwrap();
    assert!(checker.loop_analysis_exhausted.is_none());
    assert_eq!(checker.loop_analysis_depth, 0);

    let invalid = parse("fn main() { while (true) { print(missing) } }");
    let error = Checker::new().check(&invalid).unwrap_err();
    assert!(error.message.contains("missing"));
    assert!(!error.message.contains("budget"));
}

#[test]
fn loop_analysis_nested_passes_share_budget_and_restore_speculation_depth() {
    let source = format!(
        // Each inner loop can fall through to its enclosing header; a nested
        // nonreturning while(true) must not manufacture speculative backedges.
        "fn main() {{ gate = true value = 1 {} print(value) gate = false {} }}",
        "while (gate) {".repeat(6),
        "}".repeat(6)
    );
    let mut checker = Checker::new();
    checker.loop_analysis_remaining = 8;
    let error = checker.check_program(&parse(&source)).unwrap_err();
    assert_eq!(
        error.message,
        "maximum check work exceeded; loop ownership analysis budget exhausted"
    );
    assert_eq!(checker.loop_analysis_remaining, 0);
    assert_eq!(checker.loop_analysis_depth, 0);
    assert_eq!(checker.loop_analysis_exhausted, Some(error.span));
    assert_eq!(checker.loop_depth, 0);
    assert!(checker.loop_break_states.is_empty());
    assert!(checker.loop_continue_states.is_empty());
}

#[test]
fn loop_analysis_budget_does_not_replace_move_or_type_checks() {
    for source in [
        "fn main() { text = \"owned\" while (true) { moved = text } }",
        "fn main() { while (true) { value: int = \"wrong\" } }",
    ] {
        let error = Checker::new().check(&parse(source)).unwrap_err();
        assert!(!error.message.contains("budget"), "{}", error.message);
    }
    let valid = parse("fn main() { text = \"owned\" while (true) { moved = text break } }");
    Checker::new().check(&valid).unwrap();
}

#[test]
fn loop_analysis_budget_is_not_refilled_between_sibling_loops() {
    let mut checker = Checker::new();
    checker.loop_analysis_remaining = 1;
    let ast = parse("fn main() { value = 1 while (false) {} while (false) {} }");
    let error = checker.check_program(&ast).unwrap_err();
    assert!(error
        .message
        .contains("loop ownership analysis budget exhausted"));
    assert_eq!(checker.loop_analysis_remaining, 0);
    assert_eq!(checker.loop_analysis_depth, 0);
}

#[test]
fn loop_analysis_generic_recheck_cannot_accept_exhaustion_in_final_empty_loop() {
    let ast = parse("fn Inspect<T>(value: T) { while (false) {} } fn main() { Inspect(1) }");
    let mut checker = Checker::new();
    checker.check_program(&ast).unwrap();
    checker.loop_analysis_remaining = 0;
    let result = checker.check_generic_instances();
    assert!(checker.loop_analysis_exhausted.is_some());
    let error =
        result.expect_err("a completed generic queue must not hide speculative budget exhaustion");
    assert_eq!(
        error.message,
        "maximum check work exceeded; loop ownership analysis budget exhausted"
    );
    assert_eq!(checker.loop_analysis_exhausted, Some(error.span));
    assert_eq!(checker.loop_analysis_remaining, 0);
    assert_eq!(checker.loop_analysis_depth, 0);
    assert_eq!(checker.check_generic_instances().unwrap_err(), error);
    let Item::Function(function) = &ast.items[0] else {
        panic!("expected source generic function")
    };
    assert_eq!(error.span, stmt_span(&function.body[0]));
}
