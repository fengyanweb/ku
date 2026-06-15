use std::collections::HashMap;

use crate::value::Value;

pub fn ok(value: Value) -> Value {
    Value::Result {
        ok: true,
        value: Box::new(value),
    }
}

pub fn err(domain: &str, code: &str, message: impl Into<String>) -> Value {
    Value::Result {
        ok: false,
        value: Box::new(error_object(domain, code, message)),
    }
}

pub fn error_object(domain: &str, code: &str, message: impl Into<String>) -> Value {
    Value::Object(HashMap::from([
        ("domain".to_string(), Value::String(domain.to_string())),
        ("code".to_string(), Value::String(code.to_string())),
        ("message".to_string(), Value::String(message.into())),
    ]))
}
