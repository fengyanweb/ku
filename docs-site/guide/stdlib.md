# 标准库

标准库模块可以单独导入，也可以从 `std` 根一次导入多个模块：

```ku
import "std.fs"
import "std.http"
import { fs, http, time, task } from "std"
```

`fs`、`http`、`config`、`task`、`pg`、`redis`、`mysql` 需要显式导入。`string`、`array`、`json`、`time`、`lexer`、`parser` 仍保留历史直接调用能力。

## fs

```ku
fn main(): null! {
    fs.write("out.txt", "hello")?
    text = fs.read("out.txt")?
    safe = fs.try_read("missing.txt")?
    return ok(null)
}
```

`fs.read/write` 返回 Result；`try_read/try_write` 是保留的同类型兼容别名。

## string

`text.len()` 计 Unicode 标量值；`text.byte_len()` 直接返回 UTF-8 字节数，不消费字符串。例如 `"中😀"` 分别为 2 个字符、7 个字节。`text.chars()` 将字符串线性拆成独立字符字符串数组；`text.slice(start, end)?` 仍使用字符下标。

## json

```ku
fn main(): null! {
    value = json.parse("{\"name\":\"Ku\"}")?
    text = json.stringify(value)?
    println(text)
    return ok(null)
}
```

`json.parse` 返回 `KuValue!`，支持 JSON object、array 和标量值。`json.stringify` 返回 `str!`；`json.try_parse` 是 `json.parse` 的同类型兼容别名。

## time

`time.now()` 返回 Unix epoch 毫秒整数。`time.instant()` 返回 `{ kind:"time.time", millis:int }`，用于日期、格式化和时间差 API：

```ku
println(time.now())
instant = time.instant()
println(time.unix(instant))
println(time.elapsed(instant))
println(time.steady_millis())
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

## 数据库

PostgreSQL、Redis 和 MySQL 目前仅由 native backend 提供，只有一种普通业务入口：`module.client(config)?`。返回的 owned client 内部自动维护有界连接池；业务代码不创建 raw connection，也不手动管理 pool。client 和 result 必须先赋给变量，再调用 receiver 方法。下列密码仅是配置占位符，实际凭据应从 secret store 或受控环境配置读取，不要提交到源码。

### PostgreSQL

```ku
import pg from "std.pg"

fn main(): null! {
    client = pg.client({
        conninfo: "host=db.example.com dbname=app user=app password=<password> sslmode=verify-full sslrootcert=/etc/ssl/certs/app-root.crt",
        max_connections: 8,
        max_waiters: 64,
        connect_timeout_ms: 5000,
        acquire_timeout_ms: 5000,
        query_timeout_ms: 30000
    })?

    result = client.query("SELECT name FROM users WHERE id = $1", ["42"])?
    println(result.rows())
    println(result.cols())
    println(result.value(0, 0)?)
    println(result.is_null(0, 0)?)

    // 无参数 SQL 仍必须传 []。
    version = client.query("SELECT version()", [])?
    println(version.value(0, 0)?)
    client.close()
    return ok(null)
}
```

配置字段固定为必填的 `conninfo`，以及可选的 `max_connections`、`max_waiters`、`connect_timeout_ms`、`acquire_timeout_ms`、`query_timeout_ms`。client receiver 只有 `query(sql, params)`、`close()`；result receiver 是 `rows()`、`cols()`、`value(row, col)`、`is_null(row, col)`。参数占位符使用 PostgreSQL 的 `$1`、`$2`，参数值放入 `[str]`，不得把值拼接进 SQL。远程连接应在 `conninfo` 中使用 `sslmode=verify-full` 和可信 `sslrootcert`。

### Redis

```ku
import redis from "std.redis"

fn main(): null! {
    client = redis.client({
        host: "127.0.0.1",
        port: 6379,
        username: "default",
        password: "<password>",
        max_connections: 8,
        max_waiters: 64,
        connect_timeout_ms: 5000,
        acquire_timeout_ms: 5000,
        command_timeout_ms: 5000
    })?

    client.ping()?
    client.set("user:42", "Ku")?
    println(client.get("user:42")?)
    println(client.exists("user:42")?)
    client.del("user:42")?
    client.close()
    return ok(null)
}
```

配置字段固定为必填的 `host`，以及可选的 `port`、`username`、`password`、`max_connections`、`max_waiters`、`connect_timeout_ms`、`acquire_timeout_ms`、`command_timeout_ms`；提供 `username` 时也必须提供 `password`。receiver 只有 `ping()`、`get(key)`、`set(key, value)`、`exists(key)`、`del(key)`、`close()`；`get` 缺键返回 `redis/key_not_found`，不会把缺键当成空字符串。

### MySQL

```ku
import mysql from "std.mysql"

fn main(): null! {
    client = mysql.client({
        host: "127.0.0.1",
        port: 3306,
        user: "app",
        password: "<password>",
        database: "app",
        max_connections: 8,
        max_waiters: 64,
        connect_timeout_ms: 5000,
        acquire_timeout_ms: 5000,
        query_timeout_ms: 30000
    })?

    result = client.query("SELECT name FROM users WHERE id = ?", ["42"])?
    println(result.rows())
    println(result.cols())
    println(result.value(0, 0)?)
    println(result.is_null(0, 0)?)

    // 无参数 SQL 仍必须传 []。
    version = client.query("SELECT VERSION()", [])?
    println(version.value(0, 0)?)

    changed = client.execute("UPDATE jobs SET checked = ? WHERE id = ?", ["1", "42"])?
    println(changed)
    client.close()
    return ok(null)
}
```

配置字段固定为必填的 `host`、`user`、`password`、`database`，以及可选的 `port`、`max_connections`、`max_waiters`、`connect_timeout_ms`、`acquire_timeout_ms`、`query_timeout_ms`。client receiver 只有 `query(sql, params)`、`execute(sql, params)`、`close()`；result receiver 是 `rows()`、`cols()`、`value(row, col)`、`is_null(row, col)`。参数占位符使用 `?`，参数值放入 `[str]`；无参数调用也必须传 `[]`。所有 SQL 都走 prepared statement，不提供字符串拼接查询的第二套接口。

Redis 和 MySQL 当前没有内建、可配置并可验证证书与主机名的 TLS。它们只应连接 loopback、可信内网，或通过已验证的受控 TLS tunnel/proxy；不要直接暴露在不可信公网。此限制不能由连接池或 timeout 替代。

数据库操作返回 `execution_unknown` 或 `execution_completed_without_result` 时，语句可能已经执行，禁止自动重试。调用方必须先按业务幂等键、事务记录或人工对账确认结果，再决定补偿动作；驱动不会自动重放 SQL。

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

`http.request` 配置字段固定为 `method`、`url`、`headers`、`body`、`timeout_ms`、`max_body_bytes`，其中 `url` 必填。`http.client` 配置字段固定为 `timeout_ms`、`max_body_bytes`、`max_idle_connections`。字段名只使用 snake_case；静态对象和运行时动态对象都会拒绝未知字段、camelCase 和拼写错误。

服务端示例：

```powershell
cargo run -- run examples\http_server.ku
```

HTTP service 必须通过函数调用创建。server 配置只使用以下固定的 snake_case 字段：

```ku
app = http.server({
    read_header_timeout_ms: 5000,
    read_body_timeout_ms: 10000,
    write_timeout_ms: 10000,
    idle_timeout_ms: 5000,
    handler_timeout_ms: 15000,
    max_body_bytes: 1000000,
    max_header_bytes: 16384,
    max_connections: 1024,
    max_active_requests: 256,
    max_pending_requests: 1024
})
```

`http.service` / `http.server` 不再作为属性式默认对象运行。配置字段只有 `read_header_timeout_ms`、`read_body_timeout_ms`、`write_timeout_ms`、`idle_timeout_ms`、`handler_timeout_ms`、`max_body_bytes`、`max_header_bytes`、`max_connections`、`max_active_requests`、`max_pending_requests`；未知字段会被拒绝。

路由参数固定使用 `/user/{id}`，不支持 `:id`。删除路由固定调用 `del`，不提供 `delete` 别名：

```ku
app = http.service()

app.get("/health", fn() {
    return http.text("ok")
})

app.get("/user/{id}", fn(req) {
    return http.json({ id: req.params.id.clone() })
})

app.del("/user/{id}", fn(req) {
    return http.json({ removed_id: req.params.id.clone() })
})
```

普通 handler 只有 `fn()` 和 `fn(req)` 两种签名，不支持 `fn(req, res)`。handler 直接返回 `http.text/json/html/empty/redirect(...)`，不接收 response writer。

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

HTTP status 是协议状态码；业务 `body.code/msg/data` 由开发者自己维护。`_req` 只保留给适配器/测试 mock 等必须带参数但暂时不用的场景。

## task

`std.task` 是 runtime 内部诊断和压力测试命名空间，不是普通开发者业务 API。业务并发只通过 `async fn` 调用返回 task，然后 `await task` / `await task?`；用户不能手动 spawn、调度或管理 task。

普通开发者压测 HTTP 服务时使用：

```powershell
ku run examples\http_capacity_10m.ku
powershell -ExecutionPolicy Bypass -File examples\http_bench.ps1 -Url http://127.0.0.1:8080/health -Requests 10000000 -Concurrency 1000 -TimeoutSeconds 600
```

根目录 `test.ku` 和 `run-test.ps1` 只作为 runtime 维护者内部诊断入口，不作为业务开发示例。
