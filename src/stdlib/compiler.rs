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
                    let output = format!("{program:#?}");
                    if output.len() > MAX_AST_OUTPUT_BYTES {
                        return Err(KuError::runtime("parser.parse output is too large", span));
                    }
                    Ok(Some(Value::String(output)))
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
