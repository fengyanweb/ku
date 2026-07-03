use crate::{
    error::KuResult,
    span::Span,
    stdlib::core::{expect_arg_count, expected_type},
    value::Value,
};

pub fn eval(function: &str, args: &[Value], span: Span) -> KuResult<Option<Value>> {
    match function {
        "get_or" => {
            expect_arg_count("object.get_or", args.len(), 3, span)?;
            let Value::Object(fields) = &args[0] else {
                return Err(expected_type("object", &args[0], span));
            };
            let Value::String(key) = &args[1] else {
                return Err(expected_type("str", &args[1], span));
            };
            Ok(Some(
                fields.get(key).cloned().unwrap_or_else(|| args[2].clone()),
            ))
        }
        _ => Ok(None),
    }
}
