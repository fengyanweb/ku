use crate::{
    error::KuResult,
    span::Span,
    stdlib::core::{expect_arg_count, expected_type},
    value::Value,
};

pub fn eval(function: &str, args: &[Value], span: Span) -> KuResult<Option<Value>> {
    match function {
        "len" => {
            let value = one_string("string.len", args, span)?;
            Ok(Some(Value::Int(value.chars().count() as i64)))
        }
        "contains" => {
            let (value, needle) = two_strings("string.contains", args, span)?;
            Ok(Some(Value::Bool(value.contains(needle))))
        }
        "starts_with" => {
            let (value, needle) = two_strings("string.starts_with", args, span)?;
            Ok(Some(Value::Bool(value.starts_with(needle))))
        }
        "ends_with" => {
            let (value, needle) = two_strings("string.ends_with", args, span)?;
            Ok(Some(Value::Bool(value.ends_with(needle))))
        }
        "trim" => {
            let value = one_string("string.trim", args, span)?;
            Ok(Some(Value::String(value.trim().to_string())))
        }
        "lower" => {
            let value = one_string("string.lower", args, span)?;
            Ok(Some(Value::String(value.to_lowercase())))
        }
        "upper" => {
            let value = one_string("string.upper", args, span)?;
            Ok(Some(Value::String(value.to_uppercase())))
        }
        "replace" => {
            expect_arg_count("string.replace", args.len(), 3, span)?;
            let Value::String(value) = &args[0] else {
                return Err(expected_type("str", &args[0], span));
            };
            let Value::String(from) = &args[1] else {
                return Err(expected_type("str", &args[1], span));
            };
            let Value::String(to) = &args[2] else {
                return Err(expected_type("str", &args[2], span));
            };
            Ok(Some(Value::String(value.replace(from, to))))
        }
        "slice" => {
            expect_arg_count("string.slice", args.len(), 3, span)?;
            let Value::String(value) = &args[0] else {
                return Err(expected_type("str", &args[0], span));
            };
            let Value::Int(start) = args[1] else {
                return Err(expected_type("int", &args[1], span));
            };
            let Value::Int(end) = args[2] else {
                return Err(expected_type("int", &args[2], span));
            };
            Ok(Some(slice(value, start, end)))
        }
        _ => Ok(None),
    }
}

fn one_string<'a>(name: &str, args: &'a [Value], span: Span) -> KuResult<&'a str> {
    expect_arg_count(name, args.len(), 1, span)?;
    let Value::String(value) = &args[0] else {
        return Err(expected_type("str", &args[0], span));
    };
    Ok(value)
}

fn two_strings<'a>(name: &str, args: &'a [Value], span: Span) -> KuResult<(&'a str, &'a str)> {
    expect_arg_count(name, args.len(), 2, span)?;
    let Value::String(left) = &args[0] else {
        return Err(expected_type("str", &args[0], span));
    };
    let Value::String(right) = &args[1] else {
        return Err(expected_type("str", &args[1], span));
    };
    Ok((left, right))
}

fn slice(value: &str, start: i64, end: i64) -> Value {
    if start < 0 || end < 0 {
        return err("string.slice indexes must be >= 0");
    }
    if start > end {
        return err("string.slice start must be <= end");
    }
    let chars = value.chars().collect::<Vec<_>>();
    let start = start as usize;
    let end = end as usize;
    if end > chars.len() {
        return err(format!(
            "string.slice end {end} out of bounds for length {}",
            chars.len()
        ));
    }
    Value::Result {
        ok: true,
        value: Box::new(Value::String(chars[start..end].iter().collect())),
    }
}

fn err(message: impl Into<String>) -> Value {
    Value::Result {
        ok: false,
        value: Box::new(Value::String(message.into())),
    }
}
