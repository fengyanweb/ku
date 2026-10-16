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

#[test]
fn expression_chains_reject_before_deep_tree_copy_or_error_drop() {
    const CHILD: &str = "KU_PARSER_EXPRESSION_TREE_CHILD";
    const COMPLETED: &str = "expression tree regression completed";
    if std::env::var_os(CHILD).is_none() {
        use crate::registry_server::native_test_harness::{run_bounded, OutputLimits};
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "parser::statement_depth_tests::expression_chains_reject_before_deep_tree_copy_or_error_drop",
                "--nocapture",
            ])
            .env(CHILD, "1");
        let output = run_bounded(
            &mut command,
            std::time::Duration::from_secs(15),
            OutputLimits::new(64 * 1024, 128 * 1024),
        )
        .expect("expression tree regression has a bounded subprocess");
        assert!(
            output.status.success(),
            "expression tree child failed: {:?}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stderr)
            .lines()
            .any(|line| line == COMPLETED));
        return;
    }
    let binary = ["+", "*", "<", "==", "&&", "||"];
    let mut expressions = binary
        .iter()
        .map(|operator| vec!["0"; 10_000].join(operator))
        .collect::<Vec<_>>();
    expressions.extend([
        format!("root{}", ".field".repeat(10_000)),
        format!("root{}", "?.field".repeat(10_000)),
        format!("root{}", "[0]".repeat(10_000)),
        format!("root{}", "()".repeat(10_000)),
        format!("root{}", "?".repeat(10_000)),
        format!("0{}+", "+0".repeat(10_000)),
    ]);
    for expression in expressions {
        let source = format!("fn main() {{ return {expression} }}");
        let mut parser = Parser::new(Lexer::new(&source).lex().unwrap());
        let error = match parser.parse_program() {
            Err(error) => error,
            Ok(_) => panic!("a long expression chain must be rejected"),
        };
        assert!(error.message.contains("expression tree depth"));
        assert_eq!(parser.parse_depth, 0);
        assert_eq!(parser.statement_depth, 0);
    }
    eprintln!("{COMPLETED}");
}

#[test]
fn expression_tree_guard_preserves_legal_method_depth_width_and_precedence() {
    // A method adds Field + Call while the checker counts its receiver once.
    let clone_chain = format!("\"kept\"{}", ".clone()".repeat(31));
    let sum = vec!["1"; 32].join("+");
    let wide = format!("[{}]", vec!["0"; 20_000].join(","));
    for source in [
        format!("fn main() {{ print({clone_chain}) }}"),
        format!("fn main(): int {{ return {sum} }}"),
        format!("fn main() {{ values = {wide} }}"),
        "fn main(): int { return 1+2*3-4/2 }".into(),
    ] {
        let mut parser = Parser::new(Lexer::new(&source).lex().unwrap());
        let program = parser.parse_program().unwrap();
        crate::checker::Checker::new().check(&program).unwrap();
    }

    // Nested parentheses split binary loops. The final structural check must
    // still see the combined tree rather than each loop's local length alone.
    let mut expression = "0".to_owned();
    for _ in 0..20 {
        expression = format!("({expression})+{}", vec!["0"; 16].join("+"));
    }
    let mut parser = Parser::new(Lexer::new(&expression).lex().unwrap());
    let error = match parser.parse_expression_only() {
        Err(error) => error,
        Ok(_) => panic!("combined expression tree must exceed its structural bound"),
    };
    assert!(error.message.contains("expression tree depth"));
    assert_eq!(parser.parse_depth, 0);
}
