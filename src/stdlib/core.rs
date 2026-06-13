use crate::{
    error::{KuError, KuResult},
    span::Span,
    value::Value,
};

pub fn eval_builtin(name: &str, args: &[Value], span: Span) -> KuResult<Option<Value>> {
    match name {
        "len" => {
            expect_arg_count(name, args.len(), 1, span)?;
            match &args[0] {
                Value::String(value) => Ok(Some(Value::Int(value.chars().count() as i64))),
                Value::Array(values) => Ok(Some(Value::Int(values.len() as i64))),
                value => Err(expected_type("str or array", value, span)),
            }
        }
        "str" => {
            expect_arg_count(name, args.len(), 1, span)?;
            Ok(Some(Value::String(args[0].to_string())))
        }
        "ok" => {
            expect_arg_count(name, args.len(), 1, span)?;
            Ok(Some(Value::Result {
                ok: true,
                value: Box::new(args[0].clone()),
            }))
        }
        "err" => {
            expect_arg_count(name, args.len(), 1, span)?;
            let Value::String(message) = &args[0] else {
                return Err(expected_type("str", &args[0], span));
            };
            Ok(Some(Value::Result {
                ok: false,
                value: Box::new(Value::String(message.clone())),
            }))
        }
        _ => Ok(None),
    }
}

pub fn expect_arg_count(name: &str, actual: usize, expected: usize, span: Span) -> KuResult<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(KuError::runtime(
            format!("function '{name}' expects {expected} arguments but got {actual}"),
            span,
        ))
    }
}

pub fn expected_type(expected: &str, actual: &Value, span: Span) -> KuError {
    KuError::runtime(
        format!(
            "type error: expected {expected} but got {}",
            actual.type_name()
        ),
        span,
    )
}
