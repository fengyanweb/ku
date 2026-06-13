use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    error::{KuError, KuResult},
    span::Span,
    stdlib::core::expect_arg_count,
    value::Value,
};

pub fn eval(function: &str, args: &[Value], span: Span) -> KuResult<Option<Value>> {
    match function {
        "now" | "unix" => {
            let label = if function == "unix" {
                "time.unix"
            } else {
                "time.now"
            };
            expect_arg_count(label, args.len(), 0, span)?;
            Ok(Some(Value::Int(now_duration(span)?.as_secs() as i64)))
        }
        "millis" => {
            expect_arg_count("time.millis", args.len(), 0, span)?;
            Ok(Some(Value::Int(now_duration(span)?.as_millis() as i64)))
        }
        _ => Ok(None),
    }
}

fn now_duration(span: Span) -> KuResult<std::time::Duration> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| KuError::runtime(format!("system time error: {err}"), span))
}
