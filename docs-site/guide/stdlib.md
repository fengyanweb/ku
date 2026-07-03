# 标准库

标准库模块可以单独导入，也可以从 `std` 根一次导入多个模块：

```ku
import "std.fs"
import "std.http"
import { fs, http, time, task } from "std"
```

`fs`、`http`、`config`、`task` 需要显式导入。`string`、`array`、`json`、`time`、`lexer`、`parser` 仍保留历史直接调用能力。

## fs

```ku
fs.write("out.txt", "hello")
text = fs.read("out.txt")
safe = fs.try_read("missing.txt")?
```

`fs.read/write` 失败是运行时错误；`try_read/try_write` 返回 Result。

## time

`time.now()` 返回 `{ kind:"time.time", millis:int }`。常用时间戳：

```ku
now = time.now()
println(time.unix(now))
println(time.millis())
```

日期、格式化、解析、时间段和 sleep：

```ku
fn main(): null! {
    t = time.parse("2026-06-23 18:30:00", "yyyy-MM-dd HH:mm:ss", "+08:00")?
    println(time.format(t, "yyyy-MM-dd HH:mm:ss", "utc")?)

    d = time.date(2026, 6, 23)?
    println(time.weekday(d))

    duration = time.duration(5, "s")?
    time.sleep(duration)?
    return ok(null)
}
```

非法日期、非法时区、非法格式和负 duration 返回 `Err({ domain:"time", code, message })`。

## http

客户端：

```ku
import { http } from "std"

fn main(): null! {
    res = http.get("https://example.com")?
    println(res.status)
    println(res.body)
    return ok(null)
}
```

服务端示例：

```powershell
cargo run -- run examples\http_server.ku
```

HTTP service 必须通过函数调用创建：

```ku
app = http.service()
server = http.server({ max_body_bytes: 4096 })
```

`http.service` / `http.server` 不再作为属性式默认对象运行。

压测：

```powershell
powershell -ExecutionPolicy Bypass -File examples\http_bench.ps1 -Url http://127.0.0.1:8080/json -Requests 10000 -Concurrency 100
```

HTTP server 有连接上限、active/pending 背压、handler timeout、idle timeout、header/body/write timeout；队列满或连接超限会立即返回 503，不无限排队。

响应 helper 支持默认状态码和显式状态码：

```ku
return http.json({ code: 0, msg: "ok", data: null })
return http.json(http.status.created, { code: 0, msg: "created", data: null })
return http.empty()
return http.html("<h1>ok</h1>")
return http.redirect("/login")
println(http.statusText(http.status.notFound))
```

HTTP status 是协议状态码；业务 `body.code/msg/data` 由开发者自己维护。普通 handler 不读请求时写 `fn()`，读取请求时写 `fn(req)`；`_req` 只保留给适配器/测试 mock 等必须带参数但暂时不用的场景。handler 不接收 `res/writer`，普通代码直接返回 `http.text/json/html/empty/redirect(...)`。

## task

`std.task` 是 runtime 诊断和压力测试命名空间，不是普通 task 句柄 API。业务并发只通过 `async fn` 调用返回 task，然后 `await task` / `await task?`；用户不能手动 spawn、调度或管理 task。

```ku
import { task, time } from "std"

fn main() {
    before = task.stats()
    report = task.stress(1000000, 15, 250)
    after = task.stats()
    println(report.peak_active)
    println(after.active_tasks)
}
```

根目录 `test.ku` 和 `run-test.ps1` 是可直接运行的百万并发需求压力测试入口。
