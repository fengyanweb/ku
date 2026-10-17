use std::fmt::{self, Write};

use crate::{
    error::{KuError, KuResult},
    lexer::Lexer,
    parser::Parser,
    span::Span,
    stdlib::core::{expect_arg_count, expected_type},
    value::Value,
};

const MAX_EMBEDDED_SOURCE_BYTES: usize = 1_000_000;
const MAX_EMBEDDED_TOKENS: usize = 100_000;
const MAX_AST_OUTPUT_BYTES: usize = 1_000_000;

pub fn eval(module: &str, function: &str, args: &[Value], span: Span) -> KuResult<Option<Value>> {
    match (module, function) {
        ("lexer", "scan") => {
            expect_arg_count("lexer.scan", args.len(), 1, span)?;
            let Value::String(text) = &args[0] else {
                return Err(expected_type("str", &args[0], span));
            };
            reject_large_embedded_source(text, span)?;
            let tokens = Lexer::new(text)
                .tokenize()?
                .into_iter()
                .take(MAX_EMBEDDED_TOKENS + 1)
                .map(|token| Value::String(format!("{:?}", token.kind)))
                .collect::<Vec<_>>();
            if tokens.len() > MAX_EMBEDDED_TOKENS {
                return Err(KuError::runtime(
                    "too many tokens; input is too large for lexer.scan",
                    span,
                ));
            }
            Ok(Some(Value::Array(tokens)))
        }
        ("parser", "parse") => {
            expect_arg_count("parser.parse", args.len(), 1, span)?;
            match &args[0] {
                Value::String(text) => {
                    reject_large_embedded_source(text, span)?;
                    let tokens = Lexer::new(text).tokenize()?;
                    if tokens.len() > MAX_EMBEDDED_TOKENS {
                        return Err(KuError::runtime(
                            "too many tokens; input is too large for parser.parse",
                            span,
                        ));
                    }
                    let program = Parser::new(tokens).parse_program()?;
                    Ok(Some(Value::String(format_ast(&program, span)?)))
                }
                Value::Array(tokens) => Ok(Some(Value::String(format!(
                    "Ast(tokens: {})",
                    tokens.len()
                )))),
                value => Err(expected_type("str or [str]", value, span)),
            }
        }
        _ => Ok(None),
    }
}

// Apply the existing output-size contract while formatting, before a deeply
// indented pattern can allocate the entire debug string. This limits output
// bytes; it is not an aggregate allocator or process-memory budget.
fn format_ast(program: &crate::ast::Program, span: Span) -> KuResult<String> {
    let mut output = AstOutput {
        text: String::new(),
        failure: None,
    };
    if write!(&mut output, "{program:#?}").is_err() {
        return Err(KuError::runtime(
            output
                .failure
                .unwrap_or("parser.parse could not format AST"),
            span,
        ));
    }
    Ok(output.text)
}

struct AstOutput {
    text: String,
    failure: Option<&'static str>,
}

impl Write for AstOutput {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if text.len() > MAX_AST_OUTPUT_BYTES - self.text.len() {
            self.failure = Some("parser.parse output is too large");
            return Err(fmt::Error);
        }
        self.text.try_reserve(text.len()).map_err(|_| {
            self.failure = Some("parser.parse AST output out of memory");
            fmt::Error
        })?;
        self.text.push_str(text);
        Ok(())
    }
}

fn reject_large_embedded_source(source: &str, span: Span) -> KuResult<()> {
    if source.len() > MAX_EMBEDDED_SOURCE_BYTES {
        Err(KuError::runtime(
            "embedded source is too large for compiler builtin",
            span,
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ast_output_keeps_format_and_rejects_exactly_one_byte_over_limit() {
        let program = Parser::new(Lexer::new("fn main() { print(1) }").tokenize().unwrap())
            .parse_program()
            .unwrap();
        assert_eq!(
            format_ast(&program, Span::default()).unwrap(),
            format!("{program:#?}")
        );
        let mut output = AstOutput {
            text: "x".repeat(MAX_AST_OUTPUT_BYTES - 1),
            failure: None,
        };
        output.write_str("x").unwrap();
        assert_eq!(output.text.len(), MAX_AST_OUTPUT_BYTES);
        assert!(output.write_str("x").is_err());
        assert_eq!(output.text.len(), MAX_AST_OUTPUT_BYTES);
        assert_eq!(output.failure, Some("parser.parse output is too large"));
    }

    #[test]
    fn parser_builtin_deep_pattern_output_rejects_without_recursive_format_or_drop() {
        const CHILD: &str = "KU_PARSER_AST_OUTPUT_CHILD";
        const COMPLETED: &str = "parser AST output regression completed";
        if std::env::var_os(CHILD).is_none() {
            use crate::registry_server::native_test_harness::{run_bounded, OutputLimits};
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command.args([
                "--exact", "stdlib::compiler::tests::parser_builtin_deep_pattern_output_rejects_without_recursive_format_or_drop", "--nocapture",
            ]).env(CHILD, "1");
            let output = run_bounded(
                &mut command,
                std::time::Duration::from_secs(15),
                OutputLimits::new(64 * 1024, 128 * 1024),
            )
            .unwrap();
            assert!(
                output.status.success(),
                "{:?}\n{}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stderr)
                .lines()
                .any(|line| line == COMPLETED));
            return;
        }
        let pattern = format!("{}0{}", "Choice.Wrap(".repeat(4_000), ")".repeat(4_000));
        let source = format!("fn main() {{ answer = match 0 {{ {pattern} => 1 _ => 0 }} }}");
        let error = eval("parser", "parse", &[Value::String(source)], Span::default()).unwrap_err();
        assert_eq!(error.message, "parser.parse output is too large");
        let shallow = eval(
            "parser",
            "parse",
            &[Value::String("fn main() {}".into())],
            Span::default(),
        )
        .unwrap();
        assert!(matches!(shallow, Some(Value::String(text)) if text.starts_with("Program")));
        eprintln!("{COMPLETED}");
    }
}
