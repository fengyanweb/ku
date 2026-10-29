---
title: Time and timers
description: Read clocks, format dates, calculate durations, and wait with std.time.
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources: ['src/stdlib/time.rs', 'src/stdlib/metadata.rs', 'docs/syntax.md']
sidebar:
  order: 6
slug: en/v0.0.18/docs/stdlib/time
---

Import with `import time from "std.time"`. Use a wall clock for dates and a monotonic clock for elapsed work.

## Parse, calculate, and format

Save as `time.ku`, then run `ku run time.ku`:

```ku
import time from "std.time"

fn main(): null! {
    started = time.steady_millis()
    parsed = time.parse("2026-06-23 12:30:00", "yyyy-MM-dd HH:mm:ss", "utc")?
    delay = time.duration(5, "s")?
    later = time.add(parsed.clone(), delay)
    println(time.format(later.clone(), "yyyy-MM-dd HH:mm:ss", "utc")?)
    println(time.millis(time.diff(later, parsed)))

    date = time.date(2026, 6, 23)?
    println(time.weekday(date))
    println(time.days_in_month(2028, 2)?)
    time.sleep(1)?
    println(time.steady_millis() >= started)
    return ok(null)
}
```

Output:

```text
2026-06-23 12:30:05
5000
2
29
true
```

The explicit clones preserve timestamps reused by later expressions; they are not a requirement of `&` borrowing. `time.add`, `time.diff`, and the formatting APIs follow their current parameter ownership rules.

## Clocks and value shapes

`time.now()` and `time.millis()` return Unix epoch **milliseconds**; `time.unix()` returns epoch **seconds**. `time.instant()` creates `{ kind: "time.time", millis: int }`. `time.elapsed(previous)` measures wall-clock milliseconds since that value, so clock adjustments can affect it.

`time.steady_millis()` is monotonic within the running process. Only differences between readings are meaningful; its origin is not a date and need not match across execution backends or process restarts.

`Time`, `Date`, and `Duration` below describe API values, not public nominal types for source annotations:

| Value    | Shape                                                    |
| -------- | -------------------------------------------------------- |
| Time     | `{ kind: "time.time", millis: int }`                     |
| Date     | `{ kind: "time.date", year: int, month: int, day: int }` |
| Duration | `{ kind: "time.duration", millis: int }`                 |

Date/time values use the supported calendar range and int64 millisecond arithmetic. Constructors reject invalid dates or overflow. They do not silently substitute the current date.

## Complete interface

`T!` returns a Result. The other calls return their value directly and can terminate execution on invalid inputs or overflow.

```text
time.now(): int
time.instant(): Time
time.elapsed(value:Time): int
time.unix(): int
time.unix(value:Time): int
time.millis(): int
time.millis(value:Time | Duration): int
time.steady_millis(): int
time.from_unix(seconds:int): Time
time.from_millis(ms:int): Time
time.date(): Date
time.date(value:Time): Date!
time.date(value:Time, zone:str): Date!
time.date(year:int, month:int, day:int): Date!
time.datetime(year:int, month:int, day:int, hour:int, minute:int, second:int): Time!
time.datetime(year:int, month:int, day:int, hour:int, minute:int, second:int, zone:str): Time!
time.format(value:Time): str
time.format(value:Time, layout:str): str!
time.format(value:Time, layout:str, zone:str): str!
time.parse(text:str): Time!
time.parse(text:str, layout:str): Time!
time.parse(text:str, layout:str, zone:str): Time!
time.duration(ms:int): Duration!
time.duration(value:int, unit:str): Duration!
time.add(value:Time, duration:Duration): Time
time.sub(value:Time, duration:Duration): Time
time.diff(left:Time, right:Time): Duration
time.compare(left:Time, right:Time): int
time.parts(value:Time): object
time.parts(value:Time, zone:str): object!
time.weekday(value:Time | Date): int
time.weekday(value:Time, zone:str): int!
time.is_leap(year:int): bool
time.days_in_month(year:int, month:int): int!
time.sleep(ms:int): null!
time.sleep(duration:Duration): null!
```

`time.date()` returns today's local date directly. `time.date(value)?` extracts a date in the local zone, equivalent to `time.date(value, "local")?`. All overloads with arguments return a Result; invalid Time objects, out-of-range timestamps, or invalid zones return errors.

`time.add` / `sub` shift an instant by a duration, `diff(left, right)` computes `left - right`, and `compare` returns -1, 0, or 1. `weekday` returns 1 for Monday through 7 for Sunday. `parts` returns `kind`, `year`, `month`, `day`, `hour`, `minute`, `second`, `millis` (the millisecond component), `weekday`, and `zone`.

## Formats and zones

The default parse/format layout is `yyyy-MM-dd HH:mm:ss`. Milliseconds are omitted unless the layout includes `SSS`.

| Token  | Meaning               |
| ------ | --------------------- |
| `yyyy` | Year                  |
| `MM`   | Month, 01–12          |
| `dd`   | Day, 01–31            |
| `HH`   | Hour, 00–23           |
| `mm`   | Minute, 00–59         |
| `ss`   | Second, 00–59         |
| `SSS`  | Milliseconds, 000–999 |

Zones support `"local"`, `"utc"` / `"UTC"`, and fixed offsets such as `"+08:00"` or `"-05:30"`. Named IANA zones such as `"Asia/Shanghai"` are not supported. A date text without an explicit zone is interpreted in the local zone. For reproducible interchange, pass an explicit zone.

`duration` accepts nonnegative integers and units `"ms"`, `"s"`, `"m"`, `"h"`, or `"d"`. A day is 24 hours, not a calendar-month operation. Negative `diff` results are possible; `sleep` rejects a negative duration.

## Waiting and errors

`time.sleep(milliseconds)?` performs one wait; it does not start a repeating timer or schedule a callback. In synchronous execution it blocks the current execution. The interpreter's async path uses its blocking-worker mechanism; the restricted native Task path supports integer-millisecond sleep through its timer mechanism. This does not provide a general Task network scheduler. No public `setTimeout` / `setInterval` API is available.

Recoverable errors have domain `time`, with codes such as `invalid_date`, `invalid_time`, `invalid_format`, `invalid_zone`, and `invalid_duration`. Handle Result-returning calls using `?` or `try/catch`.

## Execution support

The full calendar/duration API above is for `ku run` and the default packaged runner. Synchronous native C currently supports `now()`, `instant()`, `elapsed(Time)`, `millis()` / `millis(Time)` for values produced by `instant()`, and `steady_millis()`. Calendar parsing, formatting, configuration-like Time objects, and synchronous sleep are not part of that native subset. Native Task has its separately restricted integer sleep path; LLVM does not provide this complete library.
