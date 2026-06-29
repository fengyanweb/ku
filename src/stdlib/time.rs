use std::{collections::HashMap, thread, time::Duration};

use chrono::{
    DateTime, Datelike, FixedOffset, Local, LocalResult, NaiveDate, NaiveDateTime, Offset,
    TimeZone, Timelike, Utc,
};

use crate::{
    error::{KuError, KuResult},
    span::Span,
    stdlib::{
        core::{expect_arg_count, expected_type},
        errors,
    },
    value::Value,
};

const DEFAULT_LAYOUT: &str = "yyyy-MM-dd HH:mm:ss";
const MILLIS_PER_SECOND: i64 = 1_000;
const MILLIS_PER_MINUTE: i64 = 60 * MILLIS_PER_SECOND;
const MILLIS_PER_HOUR: i64 = 60 * MILLIS_PER_MINUTE;
const MILLIS_PER_DAY: i64 = 24 * MILLIS_PER_HOUR;

pub fn eval(function: &str, args: &[Value], span: Span) -> KuResult<Option<Value>> {
    let value = match function {
        "now" => {
            if args.is_empty() {
                time_value(now_millis(span)?)
            } else {
                expect_arg_count("time.now", args.len(), 1, span)?;
                let millis = now_millis(span)? - expect_time(&args[0], span)?;
                Value::Int(millis)
            }
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
        "format" => eval_format(args, span),
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
        "parts" => eval_parts(args, span),
        "weekday" => eval_weekday(args, span),
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
        0 => date_value_from_local(Local::now()),
        1 => {
            let Ok(millis) = expect_time(&args[0], span) else {
                return result(Err(time_error("invalid_time", "expected Time object")));
            };
            result(local_from_millis(millis, span).map(date_value_from_local))
        }
        2 => result((|| {
            let millis = expect_time(&args[0], span)?;
            let zone = expect_str(&args[1], span)?;
            let date = zoned_datetime(millis, zone, span)?;
            Ok(date_value(date.year(), date.month(), date.day()))
        })()),
        3 => result((|| {
            let year = expect_i32(&args[0], span)?;
            let month = expect_u32(&args[1], span)?;
            let day = expect_u32(&args[2], span)?;
            validate_date(year, month, day, span)?;
            Ok(date_value(year, month, day))
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
        let naive = make_naive_datetime(year, month, day, hour, minute, second, span)?;
        let millis = if let Some(zone) = args.get(6) {
            let zone = expect_str(zone, span)?;
            datetime_in_zone(naive, zone, span)?
        } else {
            local_datetime_millis(naive, span)?
        };
        Ok(time_value(millis))
    })())
}

fn eval_format(args: &[Value], span: Span) -> Value {
    match args.len() {
        1 => match expect_time(&args[0], span) {
            Ok(millis) => match format_local(millis, DEFAULT_LAYOUT, span) {
                Ok(text) => Value::String(text),
                Err(err) => result(Err(err)),
            },
            Err(err) => result(Err(err)),
        },
        2 | 3 => result((|| {
            let millis = expect_time(&args[0], span)?;
            let layout = expect_str(&args[1], span)?;
            let text = if let Some(zone) = args.get(2) {
                let zone = expect_str(zone, span)?;
                format_zoned(millis, layout, zone, span)?
            } else {
                format_local(millis, layout, span)?
            };
            Ok(Value::String(text))
        })()),
        _ => result(Err(time_error(
            "invalid_arguments",
            format!(
                "function 'time.format' expects 1, 2, or 3 arguments but got {}",
                args.len()
            ),
        ))),
    }
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
        let chrono_layout = chrono_layout(layout);
        let naive = NaiveDateTime::parse_from_str(text, &chrono_layout)
            .map_err(|err| time_error("invalid_format", format!("invalid time text: {err}")))?;
        let millis = if let Some(zone) = args.get(2) {
            let zone = expect_str(zone, span)?;
            datetime_in_zone(naive, zone, span)?
        } else {
            local_datetime_millis(naive, span)?
        };
        Ok(time_value(millis))
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

fn eval_parts(args: &[Value], span: Span) -> Value {
    match args.len() {
        1 => match expect_time(&args[0], span) {
            Ok(millis) => match local_from_millis(millis, span) {
                Ok(local) => parts_value(local, "local"),
                Err(err) => result(Err(err)),
            },
            Err(err) => result(Err(err)),
        },
        2 => result((|| {
            let millis = expect_time(&args[0], span)?;
            let zone = expect_str(&args[1], span)?;
            Ok(parts_value(zoned_datetime(millis, zone, span)?, zone))
        })()),
        _ => result(Err(time_error(
            "invalid_arguments",
            format!(
                "function 'time.parts' expects 1 or 2 arguments but got {}",
                args.len()
            ),
        ))),
    }
}

fn eval_weekday(args: &[Value], span: Span) -> Value {
    match args.len() {
        1 => match weekday_value(&args[0], None, span) {
            Ok(day) => Value::Int(day),
            Err(err) => result(Err(err)),
        },
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
    }
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

fn date_value_from_local(value: DateTime<Local>) -> Value {
    date_value(value.year(), value.month(), value.day())
}

fn parts_value<Tz: TimeZone>(value: DateTime<Tz>, zone: &str) -> Value
where
    Tz::Offset: std::fmt::Display,
{
    Value::Object(HashMap::from([
        ("kind".to_string(), Value::String("time.parts".to_string())),
        ("year".to_string(), Value::Int(i64::from(value.year()))),
        ("month".to_string(), Value::Int(i64::from(value.month()))),
        ("day".to_string(), Value::Int(i64::from(value.day()))),
        ("hour".to_string(), Value::Int(i64::from(value.hour()))),
        ("minute".to_string(), Value::Int(i64::from(value.minute()))),
        ("second".to_string(), Value::Int(i64::from(value.second()))),
        (
            "millis".to_string(),
            Value::Int(i64::from(value.timestamp_subsec_millis())),
        ),
        (
            "weekday".to_string(),
            Value::Int(i64::from(value.weekday().number_from_monday())),
        ),
        ("zone".to_string(), Value::String(zone.to_string())),
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
    i32::try_from(expect_int(value, span)?)
        .map_err(|_| KuError::runtime("int is outside supported date range", span))
}

fn expect_u32(value: &Value, span: Span) -> KuResult<u32> {
    u32::try_from(expect_int(value, span)?)
        .map_err(|_| KuError::runtime("int must be non-negative", span))
}

fn expect_str(value: &Value, span: Span) -> KuResult<&str> {
    match value {
        Value::String(value) => Ok(value),
        other => Err(expected_type("str", other, span)),
    }
}

fn validate_date(year: i32, month: u32, day: u32, span: Span) -> KuResult<NaiveDate> {
    NaiveDate::from_ymd_opt(year, month, day).ok_or_else(|| {
        KuError::structured(
            crate::error::KuErrorKind::Runtime,
            "time",
            "invalid_date",
            format!("invalid date: {year:04}-{month:02}-{day:02}"),
            span,
        )
    })
}

fn make_naive_datetime(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    span: Span,
) -> KuResult<NaiveDateTime> {
    let date = validate_date(year, month, day, span)?;
    date.and_hms_opt(hour, minute, second).ok_or_else(|| {
        KuError::structured(
            crate::error::KuErrorKind::Runtime,
            "time",
            "invalid_time",
            format!(
                "invalid time: {year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}"
            ),
            span,
        )
    })
}

fn local_datetime_millis(value: NaiveDateTime, span: Span) -> KuResult<i64> {
    match Local.from_local_datetime(&value) {
        LocalResult::Single(value) => Ok(value.timestamp_millis()),
        LocalResult::Ambiguous(_, _) => Err(time_error_at(
            "invalid_time",
            "local time is ambiguous",
            span,
        )),
        LocalResult::None => Err(time_error_at(
            "invalid_time",
            "local time does not exist",
            span,
        )),
    }
}

fn datetime_in_zone(value: NaiveDateTime, zone: &str, span: Span) -> KuResult<i64> {
    match zone_kind(zone, span)? {
        ZoneKind::Local => local_datetime_millis(value, span),
        ZoneKind::Offset(offset) => offset
            .from_local_datetime(&value)
            .single()
            .map(|value| value.timestamp_millis())
            .ok_or_else(|| time_error_at("invalid_time", "invalid time for zone", span)),
    }
}

fn zoned_datetime(millis: i64, zone: &str, span: Span) -> KuResult<DateTime<FixedOffset>> {
    let utc = Utc.timestamp_millis_opt(millis).single().ok_or_else(|| {
        time_error_at("invalid_time", "time millis outside supported range", span)
    })?;
    match zone_kind(zone, span)? {
        ZoneKind::Local => {
            let local = utc.with_timezone(&Local);
            let offset = local.offset().fix();
            Ok(local.with_timezone(&offset))
        }
        ZoneKind::Offset(offset) => Ok(utc.with_timezone(&offset)),
    }
}

fn validate_time_millis(millis: i64, span: Span) -> KuResult<()> {
    Utc.timestamp_millis_opt(millis)
        .single()
        .map(|_| ())
        .ok_or_else(|| time_error_at("invalid_time", "time millis outside supported range", span))
}

fn local_from_millis(millis: i64, span: Span) -> KuResult<DateTime<Local>> {
    Ok(Utc
        .timestamp_millis_opt(millis)
        .single()
        .ok_or_else(|| time_error_at("invalid_time", "time millis outside supported range", span))?
        .with_timezone(&Local))
}

fn format_local(millis: i64, layout: &str, span: Span) -> KuResult<String> {
    Ok(local_from_millis(millis, span)?
        .format(&chrono_layout(layout))
        .to_string())
}

fn format_zoned(millis: i64, layout: &str, zone: &str, span: Span) -> KuResult<String> {
    Ok(zoned_datetime(millis, zone, span)?
        .format(&chrono_layout(layout))
        .to_string())
}

enum ZoneKind {
    Local,
    Offset(FixedOffset),
}

fn zone_kind(zone: &str, span: Span) -> KuResult<ZoneKind> {
    match zone {
        "local" => Ok(ZoneKind::Local),
        "utc" | "UTC" => FixedOffset::east_opt(0)
            .map(ZoneKind::Offset)
            .ok_or_else(|| time_error_at("invalid_zone", "invalid zone: utc", span)),
        _ => parse_fixed_offset(zone)
            .map(ZoneKind::Offset)
            .ok_or_else(|| time_error_at("invalid_zone", format!("invalid zone: {zone}"), span)),
    }
}

fn parse_fixed_offset(zone: &str) -> Option<FixedOffset> {
    if zone.len() != 6 {
        return None;
    }
    let sign = match &zone[0..1] {
        "+" => 1,
        "-" => -1,
        _ => return None,
    };
    if &zone[3..4] != ":" {
        return None;
    }
    let hours = zone[1..3].parse::<i32>().ok()?;
    let minutes = zone[4..6].parse::<i32>().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    FixedOffset::east_opt(sign * (hours * 3600 + minutes * 60))
}

fn chrono_layout(layout: &str) -> String {
    layout
        .replace("yyyy", "%Y")
        .replace("SSS", "%.3f")
        .replace("MM", "%m")
        .replace("dd", "%d")
        .replace("HH", "%H")
        .replace("mm", "%M")
        .replace("ss", "%S")
}

fn weekday_value(value: &Value, zone: Option<&str>, span: Span) -> KuResult<i64> {
    match object_kind(value) {
        Some("time.date") => {
            let Value::Object(fields) = value else {
                unreachable!();
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
            Ok(i64::from(
                validate_date(year, month, day, span)?
                    .weekday()
                    .number_from_monday(),
            ))
        }
        Some("time.time") => {
            let millis = expect_time(value, span)?;
            let weekday = if let Some(zone) = zone {
                zoned_datetime(millis, zone, span)?
                    .weekday()
                    .number_from_monday()
            } else {
                local_from_millis(millis, span)?
                    .weekday()
                    .number_from_monday()
            };
            Ok(i64::from(weekday))
        }
        _ => Err(expected_type("Time or Date", value, span)),
    }
}

fn days_in_month(year: i32, month: u32, span: Span) -> KuResult<i64> {
    if month == 0 || month > 12 {
        return Err(time_error_at(
            "invalid_date",
            format!("invalid month: {month}"),
            span,
        ));
    }
    let next = if month == 12 {
        validate_date(year + 1, 1, 1, span)?
    } else {
        validate_date(year, month + 1, 1, span)?
    };
    let current = validate_date(year, month, 1, span)?;
    Ok((next - current).num_days())
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
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
