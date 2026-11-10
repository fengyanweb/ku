---
title: 时间与计时
description: 使用 std.time 读取时钟、处理日期格式、计算时长和等待。
kuVersion: '0.0.19'
verifiedAt: '2026-10-23'
sources: ['src/stdlib/time.rs', 'src/stdlib/metadata.rs', 'docs/syntax.md']
sidebar:
  order: 6
---

通过 `import time from "std.time"` 导入。日期使用墙上时钟，测量一段工作的耗时使用单调时钟。

## 解析、计算与格式化

保存为 `time.ku`，执行 `ku run time.ku`：

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

输出：

```text
2026-06-23 12:30:05
5000
2
29
true
```

示例显式复制要在后续表达式中复用的时间值；这不是 `&` 借用的要求。`time.add`、`time.diff` 和格式化 API 遵循当前各自的参数所有权规则。

## 时钟与值结构

`time.now()`、`time.millis()` 返回 Unix epoch **毫秒**，`time.unix()` 返回 epoch **秒**。`time.instant()` 创建 `{ kind: "time.time", millis: int }`。`time.elapsed(previous)` 使用墙上时钟计算毫秒差，会受到系统时间调整影响。

`time.steady_millis()` 在当前进程内单调递增。只有两次读取的差值有意义；它的起点不是日期，也不保证在不同后端或进程重启后相同。

下列 `Time`、`Date`、`Duration` 是 API 值的说明名称，不是可以直接用于源码注解的公开名义类型：

| 值       | 结构                                                     |
| -------- | -------------------------------------------------------- |
| Time     | `{ kind: "time.time", millis: int }`                     |
| Date     | `{ kind: "time.date", year: int, month: int, day: int }` |
| Duration | `{ kind: "time.duration", millis: int }`                 |

日期时间遵循后端可表示的日历范围和 int64 毫秒运算。非法日期或溢出会报错，不会悄悄换成当前时间。

## 完整接口

`T!` 返回 Result，其余调用直接返回值；对非法输入或溢出，直接返回值的接口可能结束本次执行。

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

`time.date()` 直接返回本地今天的日期；`time.date(value)?` 按本地时区提取日期，与 `time.date(value, "local")?` 等价。带参数的 `date` 重载均返回 Result，非法 Time、超出日历范围或非法时区会返回错误。

`time.add/sub` 按时长移动时间点，`diff(left, right)` 计算 `left - right`，`compare` 返回 -1、0 或 1。`weekday` 用 1 表示周一、7 表示周日。`parts` 返回 `kind`、`year`、`month`、`day`、`hour`、`minute`、`second`、`millis`（毫秒分量）、`weekday`、`zone`。

## 格式与时区

默认解析和格式化模板为 `yyyy-MM-dd HH:mm:ss`；需要保留毫秒时在模板中加入 `SSS`。

| 格式符 | 含义          |
| ------ | ------------- |
| `yyyy` | 年            |
| `MM`   | 月，01–12     |
| `dd`   | 日，01–31     |
| `HH`   | 时，00–23     |
| `mm`   | 分，00–59     |
| `ss`   | 秒，00–59     |
| `SSS`  | 毫秒，000–999 |

时区支持 `"local"`、`"utc"` / `"UTC"`，以及 `"+08:00"`、`"-05:30"` 等固定偏移，不支持 `"Asia/Shanghai"` 等 IANA 时区名称。未指定时区的文本按本地时区解析；需要确定的交换格式时应显式指定时区。

`duration` 接受非负整数，单位为 `"ms"`、`"s"`、`"m"`、`"h"`、`"d"`。一天固定为 24 小时，不表示日历月运算。`diff` 可以为负，`sleep` 不接受负时长。

## 等待与错误

`time.sleep(milliseconds)?` 只等待一次，不创建周期计时器或回调调度。同步执行会阻塞当前执行；解释器 async 路径使用其 blocking-worker 机制，受限 native Task 路径通过 timer 机制支持整数毫秒等待。这不代表通用 Task 网络调度已经完成。目前没有公开的 `setTimeout/setInterval` API。

可恢复错误的 `domain` 为 `time`，常见 `code` 为 `invalid_date`、`invalid_time`、`invalid_format`、`invalid_zone`、`invalid_duration`。返回 Result 的调用使用 `?` 或 `try/catch` 处理。

## 执行支持

完整日历和时长 API 支持 `ku run` 与默认打包 runner。同步 native C 当前支持 `now()`、`instant()`、`elapsed(Time)`、`millis()` / `millis(Time)`（Time 来自 `instant()`）和 `steady_millis()`，不支持完整日历解析、格式化、自造 Time 对象与同步 sleep。native Task 有单独受限的整数 sleep 路径；LLVM 不提供完整时间库。

<span id="native-task-wait"></span>

## Native Task 等待

保存为 `task-sleep.ku`。执行 `ku check task-sleep.ku` 与 `ku build --backend c --profile release task-sleep.ku -o task-sleep`，选择受支持的整数定时路径：

```ku
import time from "std.time"

async fn main(): null! {
    time.sleep(10)?
    println("awake")
    return ok(null)
}
```

运行程序，等待可完成后输出 `awake`。10 毫秒是请求的等待，不保证精确调度时刻或延迟。此处 native Task 接受整数毫秒，不接受通用 `Duration` 对象重载，也不支持在 Task `finally` 中使用。返回 `null!`，用 `?` 处理，不写 `await`。取消和清理保留既有绝对截止期限。

交换日期使用墙钟 epoch 值，本进程耗时使用单调差值。不能把 `steady_millis` 当日期、跨进程时间戳，或在没有明确降低支持时作为 native Task 操作。
