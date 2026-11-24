use std::{
    collections::HashMap,
    sync::OnceLock,
    thread,
    time::{Duration, Instant},
};

use chrono::Utc;

#[path = "time_calendar.rs"]
mod calendar;

use crate::{
    error::{KuError, KuResult},
    span::Span,
    stdlib::{
        core::{expect_arg_count, expected_type},
        errors,
    },
    value::Value,
};

const DEFAULT_LAYOUT: &str = calendar::DEFAULT_LAYOUT;
const MILLIS_PER_SECOND: i64 = 1_000;
const MILLIS_PER_MINUTE: i64 = 60 * MILLIS_PER_SECOND;
const MILLIS_PER_HOUR: i64 = 60 * MILLIS_PER_MINUTE;
const MILLIS_PER_DAY: i64 = 24 * MILLIS_PER_HOUR;
static STEADY_START: OnceLock<Instant> = OnceLock::new();

pub fn eval(function: &str, args: &[Value], span: Span) -> KuResult<Option<Value>> {
    let value = match function {
        "now" => {
            expect_arg_count("time.now", args.len(), 0, span)?;
            Value::Int(now_millis(span)?)
        }
        "instant" => {
            expect_arg_count("time.instant", args.len(), 0, span)?;
            time_value(now_millis(span)?)
        }
        "elapsed" => {
            expect_arg_count("time.elapsed", args.len(), 1, span)?;
            let previous = expect_time(&args[0], span)?;
            Value::Int(checked_sub(
                now_millis(span)?,
                previous,
                "invalid_time",
                span,
            )?)
        }
        "unix" => match args.len() {
            0 => Value::Int(now_millis(span)? / MILLIS_PER_SECOND),
            1 => Value::Int(expect_time(&args[0], span)? / MILLIS_PER_SECOND),
            _ => {
                return Err(KuError::runtime(
                    format!(
                        "function 'time.unix' expects 0 or 1 arguments but got {}",
                        args.len()
                    ),
                    span,
                ))
            }
        },
        "millis" => match args.len() {
            0 => Value::Int(now_millis(span)?),
            1 => Value::Int(expect_time_or_duration_millis(&args[0], span)?),
            _ => {
                return Err(KuError::runtime(
                    format!(
                        "function 'time.millis' expects 0 or 1 arguments but got {}",
                        args.len()
                    ),
                    span,
                ))
            }
        },
        "steady_millis" => {
            expect_arg_count("time.steady_millis", args.len(), 0, span)?;
            let elapsed = STEADY_START.get_or_init(Instant::now).elapsed().as_millis();
            Value::Int(i64::try_from(elapsed).unwrap_or(i64::MAX))
        }
        "from_unix" => {
            expect_arg_count("time.from_unix", args.len(), 1, span)?;
            let seconds = expect_int(&args[0], span)?;
            time_value(checked_mul(
                seconds,
                MILLIS_PER_SECOND,
                "invalid_time",
                span,
            )?)
        }
        "from_millis" => {
            expect_arg_count("time.from_millis", args.len(), 1, span)?;
            let millis = expect_int(&args[0], span)?;
            validate_time_millis(millis, span)?;
            time_value(millis)
        }
        "date" => eval_date(args, span),
        "datetime" => eval_datetime(args, span),
        "format" => eval_format(args, span)?,
        "parse" => eval_parse(args, span),
        "duration" => eval_duration(args, span),
        "add" => {
            expect_arg_count("time.add", args.len(), 2, span)?;
            let millis = checked_add(
                expect_time(&args[0], span)?,
                expect_duration(&args[1], span)?,
                "invalid_time",
                span,
            )?;
            time_value(millis)
        }
        "sub" => {
            expect_arg_count("time.sub", args.len(), 2, span)?;
            let millis = checked_sub(
                expect_time(&args[0], span)?,
                expect_duration(&args[1], span)?,
                "invalid_time",
                span,
            )?;
            time_value(millis)
        }
        "diff" => {
            expect_arg_count("time.diff", args.len(), 2, span)?;
            duration_value(checked_sub(
                expect_time(&args[0], span)?,
                expect_time(&args[1], span)?,
                "invalid_duration",
                span,
            )?)
        }
        "compare" => {
            expect_arg_count("time.compare", args.len(), 2, span)?;
            let left = expect_time(&args[0], span)?;
            let right = expect_time(&args[1], span)?;
            Value::Int(match left.cmp(&right) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            })
        }
        "parts" => eval_parts(args, span)?,
        "weekday" => eval_weekday(args, span)?,
        "is_leap" => {
            expect_arg_count("time.is_leap", args.len(), 1, span)?;
            Value::Bool(is_leap_year(expect_int(&args[0], span)?))
        }
        "days_in_month" => {
            expect_arg_count("time.days_in_month", args.len(), 2, span)?;
            let year = expect_i32(&args[0], span)?;
            let month = expect_u32(&args[1], span)?;
            result(days_in_month(year, month, span).map(Value::Int))
        }
        "sleep" => {
            expect_arg_count("time.sleep", args.len(), 1, span)?;
            let millis = match &args[0] {
                Value::Int(ms) => *ms,
                value => expect_duration(value, span)?,
            };
            if millis < 0 {
                return Ok(Some(errors::err(
                    "time",
                    "invalid_duration",
                    "duration must be >= 0",
                )));
            }
            let millis = u64::try_from(millis).map_err(|_| {
                KuError::runtime("time.sleep duration is too large for this platform", span)
            })?;
            thread::sleep(Duration::from_millis(millis));
            errors::ok(Value::Null)
        }
        _ => return Ok(None),
    };
    Ok(Some(value))
}

fn eval_date(args: &[Value], span: Span) -> Value {
    match args.len() {
        0 => date_from_calendar(calendar::date_now()),
        1 => {
            let Ok(millis) = expect_time(&args[0], span) else {
                return result(Err(time_error("invalid_time", "expected Time object")));
            };
            result(calendar_result(calendar::date_at(millis, None), span).map(date_from_calendar))
        }
        2 => result((|| {
            let millis = expect_time(&args[0], span)?;
            let zone = expect_str(&args[1], span)?;
            Ok(date_from_calendar(calendar_result(
                calendar::date_at(millis, Some(zone)),
                span,
            )?))
        })()),
        3 => result((|| {
            let year = expect_i32(&args[0], span)?;
            let month = expect_u32(&args[1], span)?;
            let day = expect_u32(&args[2], span)?;
            Ok(date_from_calendar(calendar_result(
                calendar::date(year, month, day),
                span,
            )?))
        })()),
        _ => result(Err(time_error(
            "invalid_arguments",
            format!(
                "function 'time.date' expects 0, 1, 2, or 3 arguments but got {}",
                args.len()
            ),
        ))),
    }
}

fn eval_datetime(args: &[Value], span: Span) -> Value {
    result((|| {
        if args.len() != 6 && args.len() != 7 {
            return Err(time_error(
                "invalid_arguments",
                format!(
                    "function 'time.datetime' expects 6 or 7 arguments but got {}",
                    args.len()
                ),
            ));
        }
        let year = expect_i32(&args[0], span)?;
        let month = expect_u32(&args[1], span)?;
        let day = expect_u32(&args[2], span)?;
        let hour = expect_u32(&args[3], span)?;
        let minute = expect_u32(&args[4], span)?;
        let second = expect_u32(&args[5], span)?;
        let zone = args
            .get(6)
            .map(|value| expect_str(value, span))
            .transpose()?;
        Ok(time_value(calendar_result(
            calendar::datetime(year, month, day, hour, minute, second, zone),
            span,
        )?))
    })())
}

fn eval_format(args: &[Value], span: Span) -> KuResult<Value> {
    Ok(match args.len() {
        1 => Value::String(calendar_result(
            calendar::format(expect_time(&args[0], span)?, DEFAULT_LAYOUT, None),
            span,
        )?),
        2 | 3 => result((|| {
            let millis = expect_time(&args[0], span)?;
            let layout = expect_str(&args[1], span)?;
            let zone = args
                .get(2)
                .map(|value| expect_str(value, span))
                .transpose()?;
            Ok(Value::String(calendar_result(
                calendar::format(millis, layout, zone),
                span,
            )?))
        })()),
        _ => result(Err(time_error(
            "invalid_arguments",
            format!(
                "function 'time.format' expects 1, 2, or 3 arguments but got {}",
                args.len()
            ),
        ))),
    })
}

fn eval_parse(args: &[Value], span: Span) -> Value {
    result((|| {
        if args.is_empty() || args.len() > 3 {
            return Err(time_error(
                "invalid_arguments",
                format!(
                    "function 'time.parse' expects 1, 2, or 3 arguments but got {}",
                    args.len()
                ),
            ));
        }
        let text = expect_str(&args[0], span)?;
        let layout = args
            .get(1)
            .map(|value| expect_str(value, span))
            .transpose()?
            .unwrap_or(DEFAULT_LAYOUT);
        let zone = args
            .get(2)
            .map(|value| expect_str(value, span))
            .transpose()?;
        Ok(time_value(calendar_result(
            calendar::parse(text, layout, zone),
            span,
        )?))
    })())
}

fn eval_duration(args: &[Value], span: Span) -> Value {
    result((|| {
        if args.len() != 1 && args.len() != 2 {
            return Err(time_error(
                "invalid_arguments",
                format!(
                    "function 'time.duration' expects 1 or 2 arguments but got {}",
                    args.len()
                ),
            ));
        }
        let value = expect_int(&args[0], span)?;
        if value < 0 {
            return Err(time_error("invalid_duration", "duration must be >= 0"));
        }
        let unit = args
            .get(1)
            .map(|value| expect_str(value, span))
            .transpose()?
            .unwrap_or("ms");
        let multiplier = match unit {
            "ms" => 1,
            "s" => MILLIS_PER_SECOND,
            "m" => MILLIS_PER_MINUTE,
            "h" => MILLIS_PER_HOUR,
            "d" => MILLIS_PER_DAY,
            _ => {
                return Err(time_error(
                    "invalid_duration",
                    format!("unsupported duration unit '{unit}'"),
                ))
            }
        };
        Ok(duration_value(checked_mul(
            value,
            multiplier,
            "invalid_duration",
            span,
        )?))
    })())
}

fn eval_parts(args: &[Value], span: Span) -> KuResult<Value> {
    Ok(match args.len() {
        1 => parts_value(calendar_result(
            calendar::parts(expect_time(&args[0], span)?, None),
            span,
        )?),
        2 => result((|| {
            let millis = expect_time(&args[0], span)?;
            let zone = expect_str(&args[1], span)?;
            Ok(parts_value(calendar_result(
                calendar::parts(millis, Some(zone)),
                span,
            )?))
        })()),
        _ => result(Err(time_error(
            "invalid_arguments",
            format!(
                "function 'time.parts' expects 1 or 2 arguments but got {}",
                args.len()
            ),
        ))),
    })
}

fn eval_weekday(args: &[Value], span: Span) -> KuResult<Value> {
    Ok(match args.len() {
        1 => Value::Int(weekday_value(&args[0], None, span)?),
        2 => result((|| {
            let zone = expect_str(&args[1], span)?;
            Ok(Value::Int(weekday_value(&args[0], Some(zone), span)?))
        })()),
        _ => result(Err(time_error(
            "invalid_arguments",
            format!(
                "function 'time.weekday' expects 1 or 2 arguments but got {}",
                args.len()
            ),
        ))),
    })
}

fn now_millis(span: Span) -> KuResult<i64> {
    let millis = Utc::now().timestamp_millis();
    if millis == i64::MIN {
        Err(KuError::runtime(
            "system time is outside Ku time range",
            span,
        ))
    } else {
        Ok(millis)
    }
}

fn time_value(millis: i64) -> Value {
    Value::Object(HashMap::from([
        ("kind".to_string(), Value::String("time.time".to_string())),
        ("millis".to_string(), Value::Int(millis)),
    ]))
}

fn date_value(year: i32, month: u32, day: u32) -> Value {
    Value::Object(HashMap::from([
        ("kind".to_string(), Value::String("time.date".to_string())),
        ("year".to_string(), Value::Int(i64::from(year))),
        ("month".to_string(), Value::Int(i64::from(month))),
        ("day".to_string(), Value::Int(i64::from(day))),
    ]))
}

fn duration_value(millis: i64) -> Value {
    Value::Object(HashMap::from([
        (
            "kind".to_string(),
            Value::String("time.duration".to_string()),
        ),
        ("millis".to_string(), Value::Int(millis)),
    ]))
}

fn date_from_calendar(date: calendar::Date) -> Value {
    date_value(date.year, date.month, date.day)
}

fn parts_value(parts: calendar::Parts) -> Value {
    Value::Object(HashMap::from([
        ("kind".to_string(), Value::String("time.parts".to_string())),
        ("year".to_string(), Value::Int(i64::from(parts.date.year))),
        ("month".to_string(), Value::Int(i64::from(parts.date.month))),
        ("day".to_string(), Value::Int(i64::from(parts.date.day))),
        ("hour".to_string(), Value::Int(i64::from(parts.hour))),
        ("minute".to_string(), Value::Int(i64::from(parts.minute))),
        ("second".to_string(), Value::Int(i64::from(parts.second))),
        ("millis".to_string(), Value::Int(i64::from(parts.millis))),
        ("weekday".to_string(), Value::Int(i64::from(parts.weekday))),
        ("zone".to_string(), Value::String(parts.zone)),
    ]))
}

fn expect_time_or_duration_millis(value: &Value, span: Span) -> KuResult<i64> {
    match object_kind(value) {
        Some("time.time") | Some("time.duration") => object_int(value, "millis", span),
        _ => Err(expected_type("Time or Duration", value, span)),
    }
}

fn expect_time(value: &Value, span: Span) -> KuResult<i64> {
    if object_kind(value) == Some("time.time") {
        object_int(value, "millis", span)
    } else {
        Err(expected_type("Time", value, span))
    }
}

fn expect_duration(value: &Value, span: Span) -> KuResult<i64> {
    if object_kind(value) == Some("time.duration") {
        object_int(value, "millis", span)
    } else {
        Err(expected_type("Duration", value, span))
    }
}

fn object_kind(value: &Value) -> Option<&str> {
    let Value::Object(fields) = value else {
        return None;
    };
    let Some(Value::String(kind)) = fields.get("kind") else {
        return None;
    };
    Some(kind)
}

fn object_int(value: &Value, name: &str, span: Span) -> KuResult<i64> {
    let Value::Object(fields) = value else {
        return Err(expected_type("object", value, span));
    };
    match fields.get(name) {
        Some(Value::Int(value)) => Ok(*value),
        Some(other) => Err(expected_type("int", other, span)),
        None => Err(KuError::runtime(
            format!("time object missing field '{name}'"),
            span,
        )),
    }
}

fn expect_int(value: &Value, span: Span) -> KuResult<i64> {
    match value {
        Value::Int(value) => Ok(*value),
        other => Err(expected_type("int", other, span)),
    }
}

fn expect_i32(value: &Value, span: Span) -> KuResult<i32> {
    calendar::expect_i32(expect_int(value, span)?)
        .map_err(|error| KuError::runtime(error.message.into_owned(), span))
}

fn expect_u32(value: &Value, span: Span) -> KuResult<u32> {
    calendar::expect_u32(expect_int(value, span)?)
        .map_err(|error| KuError::runtime(error.message.into_owned(), span))
}

fn expect_str(value: &Value, span: Span) -> KuResult<&str> {
    match value {
        Value::String(value) => Ok(value),
        other => Err(expected_type("str", other, span)),
    }
}

fn calendar_result<T>(result: calendar::CalendarResult<T>, span: Span) -> KuResult<T> {
    result.map_err(|error| time_error_at(error.code, error.message.into_owned(), span))
}

fn validate_time_millis(millis: i64, span: Span) -> KuResult<()> {
    calendar_result(calendar::validate_time_millis(millis), span)
}

fn weekday_value(value: &Value, zone: Option<&str>, span: Span) -> KuResult<i64> {
    match object_kind(value) {
        Some("time.date") => {
            let Value::Object(fields) = value else {
                unreachable!()
            };
            let year = match fields.get("year") {
                Some(value) => expect_i32(value, span)?,
                None => return Err(KuError::runtime("date object missing year", span)),
            };
            let month = match fields.get("month") {
                Some(value) => expect_u32(value, span)?,
                None => return Err(KuError::runtime("date object missing month", span)),
            };
            let day = match fields.get("day") {
                Some(value) => expect_u32(value, span)?,
                None => return Err(KuError::runtime("date object missing day", span)),
            };
            calendar_result(calendar::weekday_date(year, month, day), span)
        }
        Some("time.time") => calendar_result(
            calendar::weekday_time(expect_time(value, span)?, zone),
            span,
        ),
        _ => Err(expected_type("Time or Date", value, span)),
    }
}

fn days_in_month(year: i32, month: u32, span: Span) -> KuResult<i64> {
    calendar_result(calendar::days_in_month(year, month), span)
}

fn is_leap_year(year: i64) -> bool {
    calendar::is_leap_year(year)
}

fn checked_add(left: i64, right: i64, code: &str, span: Span) -> KuResult<i64> {
    left.checked_add(right)
        .ok_or_else(|| time_error_at(code, "time calculation overflow", span))
}

fn checked_sub(left: i64, right: i64, code: &str, span: Span) -> KuResult<i64> {
    left.checked_sub(right)
        .ok_or_else(|| time_error_at(code, "time calculation overflow", span))
}

fn checked_mul(left: i64, right: i64, code: &str, span: Span) -> KuResult<i64> {
    left.checked_mul(right)
        .ok_or_else(|| time_error_at(code, "time calculation overflow", span))
}

fn result(value: KuResult<Value>) -> Value {
    match value {
        Ok(value) => errors::ok(value),
        Err(err) => errors::err(
            "time",
            err.code.as_deref().unwrap_or("invalid_time"),
            err.message,
        ),
    }
}

fn time_error(code: &str, message: impl Into<String>) -> KuError {
    time_error_at(code, message, Span::default())
}

fn time_error_at(code: &str, message: impl Into<String>, span: Span) -> KuError {
    KuError::structured(
        crate::error::KuErrorKind::Runtime,
        "time",
        code,
        message,
        span,
    )
}
