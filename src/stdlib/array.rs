use crate::{
    error::KuResult,
    span::Span,
    stdlib::core::{expect_arg_count, expected_type},
    value::Value,
};

pub fn eval(function: &str, args: &[Value], span: Span) -> KuResult<Option<Value>> {
    match function {
        "len" => {
            let values = one_array("array.len", args, span)?;
            Ok(Some(Value::Int(values.len() as i64)))
        }
        "is_empty" => {
            let values = one_array("array.is_empty", args, span)?;
            Ok(Some(Value::Bool(values.is_empty())))
        }
        "push" => {
            expect_arg_count("array.push", args.len(), 2, span)?;
            let Value::Array(values) = &args[0] else {
                return Err(expected_type("array", &args[0], span));
            };
            let mut next = values.clone();
            next.push(args[1].clone());
            Ok(Some(Value::Array(next)))
        }
        "concat" => {
            expect_arg_count("array.concat", args.len(), 2, span)?;
            let Value::Array(left) = &args[0] else {
                return Err(expected_type("array", &args[0], span));
            };
            let Value::Array(right) = &args[1] else {
                return Err(expected_type("array", &args[1], span));
            };
            let mut next = Vec::with_capacity(left.len() + right.len());
            next.extend(left.iter().cloned());
            next.extend(right.iter().cloned());
            Ok(Some(Value::Array(next)))
        }
        "first" => {
            let values = one_array("array.first", args, span)?;
            Ok(Some(values.first().cloned().unwrap_or(Value::Null)))
        }
        "last" => {
            let values = one_array("array.last", args, span)?;
            Ok(Some(values.last().cloned().unwrap_or(Value::Null)))
        }
        _ => Ok(None),
    }
}

fn one_array<'a>(name: &str, args: &'a [Value], span: Span) -> KuResult<&'a [Value]> {
    expect_arg_count(name, args.len(), 1, span)?;
    let Value::Array(values) = &args[0] else {
        return Err(expected_type("array", &args[0], span));
    };
    Ok(values)
}
