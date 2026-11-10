//! Browser clocks are real host clocks; native scheduling is not emulated.
use crate::{error::KuResult, span::Span, stdlib::core::expect_arg_count, value::Value};
use std::sync::OnceLock;
use web_time::{Instant, SystemTime, UNIX_EPOCH};
static START: OnceLock<Instant> = OnceLock::new();
pub fn eval(function: &str, args: &[Value], span: Span) -> KuResult<Option<Value>> {
    match function {
        "now" | "millis" | "unix" | "steady_millis" if args.is_empty() => {
            expect_arg_count(function, args.len(), 0, span)?;
            let millis = if function == "steady_millis" {
                START.get_or_init(Instant::now).elapsed().as_millis()
            } else {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| crate::browser::unsupported("host clock before Unix epoch", span))?
                    .as_millis()
            };
            Ok(Some(Value::Int(
                (if function == "unix" {
                    millis / 1000
                } else {
                    millis
                })
                .min(i64::MAX as u128) as i64,
            )))
        }
        _ => Err(crate::browser::unsupported(
            &format!("time.{function}"),
            span,
        )),
    }
}
