//! Calendar operations shared by the interpreter's value adapter and the
//! native C ABI. This module has no dependency on Ku values, AST or execution.
use std::{borrow::Cow, fmt};

use chrono::{
    DateTime, Datelike, FixedOffset, Local, LocalResult, NaiveDate, NaiveDateTime, Offset,
    TimeZone, Timelike, Utc,
};

pub const DEFAULT_LAYOUT: &str = "yyyy-MM-dd HH:mm:ss";

#[derive(Debug)]
pub struct CalendarError {
    pub code: &'static str,
    pub message: Cow<'static, str>,
}

pub type CalendarResult<T> = Result<T, CalendarError>;

impl CalendarError {
    pub fn new(code: &'static str, message: &'static str) -> Self {
        Self {
            code,
            message: Cow::Borrowed(message),
        }
    }

    pub fn out_of_memory() -> Self {
        Self::new("out_of_memory", "time allocation failed")
    }
}

/// A fallible formatter avoids String's infallible growth/formatting panic.
/// It imposes no new public length cap. A failed partial output is dropped.
struct Text {
    value: String,
    allocation_failed: bool,
}

impl Text {
    fn new() -> Self {
        Self {
            value: String::new(),
            allocation_failed: false,
        }
    }
}

impl fmt::Write for Text {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if self.value.try_reserve(text.len()).is_err() {
            self.allocation_failed = true;
            return Err(fmt::Error);
        }
        self.value.push_str(text);
        Ok(())
    }
}

fn error_format(code: &'static str, arguments: fmt::Arguments<'_>) -> CalendarError {
    let mut text = Text::new();
    if fmt::write(&mut text, arguments).is_err() {
        return CalendarError::out_of_memory();
    }
    CalendarError {
        code,
        message: Cow::Owned(text.value),
    }
}

fn owned_text(text: &str) -> CalendarResult<String> {
    let mut value = String::new();
    value
        .try_reserve_exact(text.len())
        .map_err(|_| CalendarError::out_of_memory())?;
    value.push_str(text);
    Ok(value)
}

fn replace(text: &str, from: &str, to: &str) -> CalendarResult<String> {
    let count = text.matches(from).count();
    let length = text
        .len()
        .checked_sub(
            count
                .checked_mul(from.len())
                .ok_or_else(CalendarError::out_of_memory)?,
        )
        .and_then(|length| {
            count
                .checked_mul(to.len())
                .and_then(|added| length.checked_add(added))
        })
        .ok_or_else(CalendarError::out_of_memory)?;
    let mut output = String::new();
    output
        .try_reserve_exact(length)
        .map_err(|_| CalendarError::out_of_memory())?;
    let mut start = 0;
    for (index, matched) in text.match_indices(from) {
        output.push_str(&text[start..index]);
        output.push_str(to);
        start = index + matched.len();
    }
    output.push_str(&text[start..]);
    Ok(output)
}

pub fn chrono_layout(layout: &str) -> CalendarResult<String> {
    // Preserve the exact existing sequential replacement rules, including the
    // interactions between an earlier replacement and a later token.
    let mut layout = owned_text(layout)?;
    for (from, to) in [
        ("yyyy", "%Y"),
        // SSS is the three digits themselves; a separator belongs to the layout.
        ("SSS", "%3f"),
        ("MM", "%m"),
        ("dd", "%d"),
        ("HH", "%H"),
        ("mm", "%M"),
        ("ss", "%S"),
    ] {
        layout = replace(&layout, from, to)?;
    }
    Ok(layout)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Date {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

impl Date {
    fn from_datetime<Tz: TimeZone>(value: &DateTime<Tz>) -> Self {
        Self {
            year: value.year(),
            month: value.month(),
            day: value.day(),
        }
    }
}

#[derive(Debug)]
pub struct Parts {
    pub date: Date,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub millis: u32,
    pub weekday: u32,
    pub zone: String,
}

fn parts_value<Tz: TimeZone>(value: DateTime<Tz>, zone: &str) -> CalendarResult<Parts> {
    Ok(Parts {
        date: Date::from_datetime(&value),
        hour: value.hour(),
        minute: value.minute(),
        second: value.second(),
        millis: value.timestamp_subsec_millis(),
        weekday: value.weekday().number_from_monday(),
        zone: owned_text(zone)?,
    })
}

pub fn expect_i32(value: i64) -> CalendarResult<i32> {
    i32::try_from(value)
        .map_err(|_| CalendarError::new("invalid_time", "int is outside supported date range"))
}

pub fn expect_u32(value: i64) -> CalendarResult<u32> {
    u32::try_from(value).map_err(|_| CalendarError::new("invalid_time", "int must be non-negative"))
}

pub fn validate_date(year: i32, month: u32, day: u32) -> CalendarResult<NaiveDate> {
    NaiveDate::from_ymd_opt(year, month, day).ok_or_else(|| {
        error_format(
            "invalid_date",
            format_args!("invalid date: {year:04}-{month:02}-{day:02}"),
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
) -> CalendarResult<NaiveDateTime> {
    validate_date(year, month, day)?
        .and_hms_opt(hour, minute, second)
        .ok_or_else(|| {
            error_format(
                "invalid_time",
                format_args!(
                    "invalid time: {year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}"
                ),
            )
        })
}

fn local_datetime_millis(value: NaiveDateTime) -> CalendarResult<i64> {
    match Local.from_local_datetime(&value) {
        LocalResult::Single(value) => Ok(value.timestamp_millis()),
        LocalResult::Ambiguous(_, _) => Err(CalendarError::new(
            "invalid_time",
            "local time is ambiguous",
        )),
        LocalResult::None => Err(CalendarError::new(
            "invalid_time",
            "local time does not exist",
        )),
    }
}

fn datetime_in_zone(value: NaiveDateTime, zone: &str) -> CalendarResult<i64> {
    match zone_kind(zone)? {
        ZoneKind::Local => local_datetime_millis(value),
        ZoneKind::Offset(offset) => offset
            .from_local_datetime(&value)
            .single()
            .map(|value| value.timestamp_millis())
            .ok_or_else(|| CalendarError::new("invalid_time", "invalid time for zone")),
    }
}

fn zoned_datetime(millis: i64, zone: &str) -> CalendarResult<DateTime<FixedOffset>> {
    let utc = Utc
        .timestamp_millis_opt(millis)
        .single()
        .ok_or_else(|| CalendarError::new("invalid_time", "time millis outside supported range"))?;
    match zone_kind(zone)? {
        ZoneKind::Local => {
            let local = utc.with_timezone(&Local);
            let offset = local.offset().fix();
            Ok(local.with_timezone(&offset))
        }
        ZoneKind::Offset(offset) => Ok(utc.with_timezone(&offset)),
    }
}

pub fn validate_time_millis(millis: i64) -> CalendarResult<()> {
    Utc.timestamp_millis_opt(millis)
        .single()
        .map(|_| ())
        .ok_or_else(|| CalendarError::new("invalid_time", "time millis outside supported range"))
}

fn local_from_millis(millis: i64) -> CalendarResult<DateTime<Local>> {
    Ok(Utc
        .timestamp_millis_opt(millis)
        .single()
        .ok_or_else(|| CalendarError::new("invalid_time", "time millis outside supported range"))?
        .with_timezone(&Local))
}

pub fn date_now() -> Date {
    Date::from_datetime(&Local::now())
}

pub fn date_at(millis: i64, zone: Option<&str>) -> CalendarResult<Date> {
    match zone {
        Some(zone) => Ok(Date::from_datetime(&zoned_datetime(millis, zone)?)),
        None => Ok(Date::from_datetime(&local_from_millis(millis)?)),
    }
}

pub fn date(year: i32, month: u32, day: u32) -> CalendarResult<Date> {
    validate_date(year, month, day)?;
    Ok(Date { year, month, day })
}

pub fn datetime(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    zone: Option<&str>,
) -> CalendarResult<i64> {
    let naive = make_naive_datetime(year, month, day, hour, minute, second)?;
    match zone {
        Some(zone) => datetime_in_zone(naive, zone),
        None => local_datetime_millis(naive),
    }
}

fn format_datetime<Tz: TimeZone>(value: DateTime<Tz>, layout: &str) -> CalendarResult<String>
where
    Tz::Offset: fmt::Display,
{
    let layout = chrono_layout(layout)?;
    let mut text = Text::new();
    if value.format(&layout).write_to(&mut text).is_err() {
        return Err(if text.allocation_failed {
            CalendarError::out_of_memory()
        } else {
            CalendarError::new("invalid_format", "invalid time format")
        });
    }
    Ok(text.value)
}

pub fn format(millis: i64, layout: &str, zone: Option<&str>) -> CalendarResult<String> {
    match zone {
        Some(zone) => format_datetime(zoned_datetime(millis, zone)?, layout),
        None => format_datetime(local_from_millis(millis)?, layout),
    }
}

pub fn parse(text: &str, layout: &str, zone: Option<&str>) -> CalendarResult<i64> {
    let layout = chrono_layout(layout)?;
    let naive = NaiveDateTime::parse_from_str(text, &layout).map_err(|error| {
        error_format("invalid_format", format_args!("invalid time text: {error}"))
    })?;
    match zone {
        Some(zone) => datetime_in_zone(naive, zone),
        None => local_datetime_millis(naive),
    }
}

pub fn parts(millis: i64, zone: Option<&str>) -> CalendarResult<Parts> {
    match zone {
        Some(zone) => parts_value(zoned_datetime(millis, zone)?, zone),
        None => parts_value(local_from_millis(millis)?, "local"),
    }
}

pub fn weekday_time(millis: i64, zone: Option<&str>) -> CalendarResult<i64> {
    match zone {
        Some(zone) => Ok(i64::from(
            zoned_datetime(millis, zone)?.weekday().number_from_monday(),
        )),
        None => Ok(i64::from(
            local_from_millis(millis)?.weekday().number_from_monday(),
        )),
    }
}

pub fn weekday_date(year: i32, month: u32, day: u32) -> CalendarResult<i64> {
    Ok(i64::from(
        validate_date(year, month, day)?
            .weekday()
            .number_from_monday(),
    ))
}

enum ZoneKind {
    Local,
    Offset(FixedOffset),
}

fn zone_kind(zone: &str) -> CalendarResult<ZoneKind> {
    match zone {
        "local" => Ok(ZoneKind::Local),
        "utc" | "UTC" => FixedOffset::east_opt(0)
            .map(ZoneKind::Offset)
            .ok_or_else(|| CalendarError::new("invalid_zone", "invalid zone: utc")),
        _ => parse_fixed_offset(zone)
            .map(ZoneKind::Offset)
            .ok_or_else(|| error_format("invalid_zone", format_args!("invalid zone: {zone}"))),
    }
}

fn parse_fixed_offset(zone: &str) -> Option<FixedOffset> {
    let bytes = zone.as_bytes();
    if bytes.len() != 6 || !bytes.is_ascii() {
        return None;
    }
    let sign = match bytes[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    if bytes[3] != b':' {
        return None;
    }
    let hours = zone[1..3].parse::<i32>().ok()?;
    let minutes = zone[4..6].parse::<i32>().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    FixedOffset::east_opt(sign * (hours * 3600 + minutes * 60))
}

pub fn days_in_month(year: i32, month: u32) -> CalendarResult<i64> {
    if month == 0 || month > 12 {
        return Err(error_format(
            "invalid_date",
            format_args!("invalid month: {month}"),
        ));
    }
    let current = validate_date(year, month, 1)?;
    let next = if month == 12 {
        validate_date(year + 1, 1, 1)?
    } else {
        validate_date(year, month + 1, 1)?
    };
    Ok((next - current).num_days())
}

pub fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_epoch_and_fixed_offsets() {
        assert_eq!(datetime(1970, 1, 1, 0, 0, 0, Some("UTC")).unwrap(), 0);
        assert_eq!(datetime(1970, 1, 1, 8, 0, 0, Some("+08:00")).unwrap(), 0);
        assert_eq!(datetime(1969, 12, 31, 19, 0, 0, Some("-05:00")).unwrap(), 0);
        assert_eq!(
            date_at(0, Some("-05:00")).unwrap(),
            Date {
                year: 1969,
                month: 12,
                day: 31
            }
        );
        let p = parts(-1, Some("UTC")).unwrap();
        assert_eq!((p.date.year, p.date.month, p.date.day), (1969, 12, 31));
        assert_eq!(
            (p.hour, p.minute, p.second, p.millis, p.weekday),
            (23, 59, 59, 999, 3)
        );
        assert_eq!(p.zone, "UTC");
        assert_eq!(weekday_time(0, Some("UTC")).unwrap(), 4);
        assert_eq!(weekday_date(2000, 1, 1).unwrap(), 6);
    }

    #[test]
    fn millisecond_token_has_no_implicit_dot_and_roundtrips() {
        let layout = "yyyy-MM-dd HH:mm:ss.SSS";
        let expected = "1970-01-01 00:00:00.123";
        assert_eq!(format(123, layout, Some("UTC")).unwrap(), expected);
        assert_eq!(parse(expected, layout, Some("UTC")).unwrap(), 123);
        assert_eq!(format(123, "SSS", Some("UTC")).unwrap(), "123");
        assert_eq!(
            format(-1, layout, Some("UTC")).unwrap(),
            "1969-12-31 23:59:59.999"
        );
        assert_eq!(
            parse("1969-12-31 23:59:59.999", layout, Some("UTC")).unwrap(),
            -1
        );
    }

    #[test]
    fn unicode_nul_and_chrono_directives_use_one_engine() {
        let layout = "中文\0yyyy-MM-dd HH:mm:ss.SSS %G-W%V-%u %:z %%";
        assert_eq!(
            format(123, layout, Some("+08:00")).unwrap(),
            "中文\01970-01-01 08:00:00.123 1970-W01-4 +08:00 %"
        );
        let layout = "前\0yyyy-MM-dd HH:mm:ss.SSS后";
        let encoded = format(123, layout, Some("UTC")).unwrap();
        assert_eq!(encoded, "前\01970-01-01 00:00:00.123后");
        assert_eq!(parse(&encoded, layout, Some("UTC")).unwrap(), 123);
        assert_eq!(chrono_layout("MMm yyyy SSS").unwrap(), "%%M %Y %3f");
    }

    #[test]
    fn gregorian_validation_and_limits_remain_explicit() {
        assert_eq!(date(2000, 2, 29).unwrap().day, 29);
        assert_eq!(date(1900, 2, 29).unwrap_err().code, "invalid_date");
        assert_eq!(days_in_month(2000, 2).unwrap(), 29);
        assert_eq!(days_in_month(1900, 2).unwrap(), 28);
        for (year, month, day) in [
            (2024, 0, 1),
            (2024, 13, 1),
            (2024, 1, 0),
            (2024, 4, 31),
            (i32::MAX, 1, 1),
        ] {
            assert_eq!(date(year, month, day).unwrap_err().code, "invalid_date");
        }
        assert_eq!(
            datetime(2024, 1, 1, 24, 0, 0, Some("UTC"))
                .unwrap_err()
                .code,
            "invalid_time"
        );
        assert_eq!(
            datetime(2024, 1, 1, 0, 60, 0, Some("UTC"))
                .unwrap_err()
                .code,
            "invalid_time"
        );
        assert_eq!(
            datetime(2024, 1, 1, 0, 0, 60, Some("UTC"))
                .unwrap_err()
                .code,
            "invalid_time"
        );
        assert_eq!(
            validate_time_millis(i64::MAX).unwrap_err().code,
            "invalid_time"
        );
        assert_eq!(expect_i32(i64::MAX).unwrap_err().code, "invalid_time");
        assert_eq!(expect_u32(-1).unwrap_err().code, "invalid_time");
    }

    #[test]
    fn invalid_layout_text_and_zone_are_errors() {
        for layout in ["%Q", "%", "%#z"] {
            assert_eq!(
                format(0, layout, Some("UTC")).unwrap_err().code,
                "invalid_format"
            );
        }
        for text in [
            "",
            "2024-02-30 12:00:00",
            "2024-01-01 12:00:00 trailing",
            "2024-01-01",
        ] {
            assert_eq!(
                parse(text, DEFAULT_LAYOUT, Some("UTC")).unwrap_err().code,
                "invalid_format"
            );
        }
        for zone in ["Europe/Paris", "+24:00", "+08:60", "+08", "+é:00", "UTC\0"] {
            assert_eq!(parts(0, Some(zone)).unwrap_err().code, "invalid_zone");
        }
        assert_eq!(
            parse("not a date", DEFAULT_LAYOUT, Some("also invalid"))
                .unwrap_err()
                .code,
            "invalid_format"
        );
    }

    #[test]
    fn local_zone_matches_implicit_local_without_environment_changes() {
        let millis = datetime(2024, 1, 15, 12, 34, 56, None).unwrap();
        assert_eq!(
            format(millis, DEFAULT_LAYOUT, None).unwrap(),
            "2024-01-15 12:34:56"
        );
        assert_eq!(
            format(millis, DEFAULT_LAYOUT, Some("local")).unwrap(),
            "2024-01-15 12:34:56"
        );
        assert_eq!(
            parse("2024-01-15 12:34:56", DEFAULT_LAYOUT, Some("local")).unwrap(),
            millis
        );
        assert_eq!(
            date_at(millis, None).unwrap(),
            date_at(millis, Some("local")).unwrap()
        );
    }
}
