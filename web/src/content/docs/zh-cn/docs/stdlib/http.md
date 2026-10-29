---
title: HTTP
description: 创建 HTTP 服务、注册路由、调用接口，以及处理请求、响应、超时和容量限制。
kuVersion: '0.0.19'
verifiedAt: '2026-10-28'
sources:
  [
    'src/stdlib/http.rs',
    'src/stdlib/metadata.rs',
    'docs/syntax.md',
    'src/backend/c_http_runtime.c',
    'src/backend/c_task_http.rs',
    'src/ir/task_lower.rs',
    'src/service_supervision.rs',
    'src/ir/mod.rs',
    'src/backend/c.rs',
    'tests/native_http_test.rs',
  ]
sidebar:
  order: 8
---

`std.http` 提供 HTTP 客户端、路由注册和完整响应对象。下面的服务端和客户端分别保存为两个文件。

下面的服务端与客户端示例使用**解释器（`ku run`）或默认 runner**。native C 支持同步 JSON 响应，但不提供模块级 HTTP 客户端。原生程序可参考[同步 native C 示例](#native-c)或[具名 async handler 示例](#具名-async-native-handler)。

## 启动服务

保存为 `server.ku`，执行 `ku run server.ku`。运行前确保端口 8080 空闲，结束时按 Ctrl+C。

```ku
import http from "std.http"

fn main(): null! {
    app = http.service({ max_body_bytes: 65536, handler_timeout_ms: 5000 })
    app.get("/health", fn() { return http.text("ok") })
    app.get("/users/{id}", fn(req) {
        return http.json({
            id: req.params.id.clone(),
            q: req.query.get_or("q", ""),
            method: req.method.clone()
        })
    })
    app.post("/echo", fn(req) { return http.text(req.body.clone()) })
    app.get("/empty", fn() { return http.empty() })
    app.get("/page", fn() { return http.html("<h1>Ku HTTP</h1>") })
    app.get("/go", fn() { return http.redirect("/health") })
    app.listen("127.0.0.1:8080")?
    return ok(null)
}
```

保持服务运行，在另一个终端调用：

```sh
curl 'http://127.0.0.1:8080/health'
curl 'http://127.0.0.1:8080/users/42?q=ku'
curl -X POST --data 'hello' 'http://127.0.0.1:8080/echo'
```

依次返回 `ok`、`{"id":"42","method":"GET","q":"ku"}` 和 `hello`。`GET /empty` 返回无响应体的 204，`/page` 返回 HTML，`/go` 重定向到 `/health`。

请求字符串由请求对象持有；示例中的 `.clone()` 让响应独立持有要返回的字段。仅读取字段不需要复制。

## 调用服务

保存为 `client.ku`，保持服务运行，再执行 `ku run client.ku`：

```ku
import http from "std.http"

fn main(): null! {
    response = http.get("http://127.0.0.1:8080/users/42?q=ku")?
    println(response.status)
    println(response.body)

    echo = http.request({
        method: "POST",
        url: "http://127.0.0.1:8080/echo",
        headers: { "Content-Type": "text/plain; charset=utf-8" },
        body: "你好 Ku",
        timeout_ms: 1000,
        max_body_bytes: 65536
    })?
    println(echo.body)

    missing = http.get("http://127.0.0.1:8080/missing")?
    println(missing.status)
    return ok(null)
}
```

输出：

```text
200
{"id":"42","method":"GET","q":"ku"}
你好 Ku
404
```

HTTP 404、500 等状态仍然返回 `Ok(HttpResponse)`，应检查 `response.status`。连接、URL、超时、响应体大小和 UTF-8 错误才返回错误结果。可用 `?` 传播或 `try/catch` 处理，参见[错误处理](/zh-cn/docs/guide/errors/)。

## 客户端接口

下列签名描述客户端接口与响应值。专门的 [native async handler 路径](#具名-async-native-handler)也接受 `HttpRequest` 和 `HttpResponse` 类型注解，但不表示模块级 HTTP 客户端可用于 native。

| 调用                           | 返回            | 作用                                           |
| ------------------------------ | --------------- | ---------------------------------------------- |
| `http.get(url:str)`            | `HttpResponse!` | 使用共享默认客户端发送 GET                     |
| `http.post(url:str, body:str)` | `HttpResponse!` | 发送 UTF-8 字符串 POST                         |
| `http.request(options)`        | `HttpResponse!` | 携带逐请求配置的 GET 或 POST                   |
| `http.client(options?)`        | object          | 创建配置对象，没有 `get/post/request` 实例方法 |

响应包含 `status:int`、字符串值的 `headers:object` 和 `body:str`。返回的 header 名称转为小写。URL 必须为完整的 `http://` 或 `https://` 地址。目前 `http.request` **只支持 GET 和 POST**，服务端支持 PUT/DELETE 不代表客户端也支持它们。

| 请求选项         | 类型         | 默认值或规则                              |
| ---------------- | ------------ | ----------------------------------------- |
| `url`            | str          | 必填                                      |
| `method`         | str          | `"GET"`；GET 或 POST，不区分大小写        |
| `headers`        | 字符串值对象 | 默认空；名称和值必须符合 HTTP header 格式 |
| `body`           | str          | 默认无请求体                              |
| `timeout_ms`     | int          | 5,000；范围 1–300,000                     |
| `max_body_bytes` | int          | 1,000,000；范围 1–16,777,216，限制响应体  |

默认客户端复用连接。`http.client` 的配置对象接受 `timeout_ms`、`max_body_bytes`、`max_idle_connections`（默认 128，范围 1–1,024），但**不会自动用于模块级请求调用**。需要实际改变超时或响应体大小时，使用上表中的逐请求选项。未知配置字段和 camelCase 拼写会被拒绝。

客户端错误的 `domain` 为 `http`，常见 `code` 为 `invalid_method`、`invalid_url`、`transport`、`read_failed`、`body_too_large`、`invalid_utf8`。选项类型错误和非法 header 可能在类型检查时，或发起请求前以运行时错误拒绝。

## 路由与请求字段

| 服务方法                      | 返回      | 作用                                  |
| ----------------------------- | --------- | ------------------------------------- |
| `app.get(path:str, handler)`  | null      | 注册 GET                              |
| `app.post(path:str, handler)` | null      | 注册 POST                             |
| `app.put(path:str, handler)`  | null      | 注册 PUT                              |
| `app.del(path:str, handler)`  | null      | 注册 DELETE，不是删除路由             |
| `app.listen(address:str)`     | `null!`   | 消费服务对象，进入阻塞请求循环        |
| `app.bind(address:str)`       | listener! | 只绑定不运行，仅解释器支持            |
| `listener.run()`              | `null!`   | 进入阻塞请求循环，仅解释器支持        |
| `listener.close()`            | `null!`   | 关闭尚未运行的 listener，仅解释器支持 |

`http.service(options?)` 和 `http.server(options?)` 构造等价的服务对象。先注册路由，再启动。路径参数写为 `/users/{id}`；`:id`、重复路由形状和不明确或非法模式会被拒绝。handler 接受零个参数 `fn()` 或一个参数 `fn(req)`，返回 `http.text/json/html/empty/redirect(...)` 的响应，或包含该响应的 Result。handler 不能修改捕获的外层变量，也不能重新绑定已注册 handler 共享的函数变量。没有第二个 writer 参数。

| 请求字段      | 类型         | 含义                  |
| ------------- | ------------ | --------------------- |
| `req.method`  | str          | HTTP 方法             |
| `req.path`    | str          | 请求路径              |
| `req.params`  | 字符串值对象 | 路径中的命名参数      |
| `req.query`   | 字符串值对象 | 解析后的查询参数      |
| `req.headers` | 字符串值对象 | 请求 header，名称小写 |
| `req.body`    | str          | UTF-8 请求体          |

解释器中可选查询参数使用 `req.query.get_or("q", "")`。普通缺键读取在解释器中报错，而 native HTTP 请求映射的字段读取目前在缺键时返回空字符串。不要依赖这两条路径一致地拒绝缺失必填字段。使用内联 `fn(req)`，检查器才能从路由上下文识别请求字段。解释器和默认 runner 的普通 handler 路径没有公开的 `HttpRequest` 名义参数类型；native async handler 的类型注解见下文。

## 响应与状态码

| helper                                                                    | 默认状态码 | 内容类型或行为                               |
| ------------------------------------------------------------------------- | ---------- | -------------------------------------------- |
| `http.text(body:str)` / `http.text(status:int, body:str)`                 | 200        | `text/plain; charset=utf-8`                  |
| `http.html(body:str)` / `http.html(status:int, body:str)`                 | 200        | `text/html; charset=utf-8`                   |
| `http.json(value)` / `http.json(status:int, value)`                       | 200        | 紧凑 JSON，`application/json; charset=utf-8` |
| `http.empty(status?:int)`                                                 | 204        | 空响应体                                     |
| `http.redirect(location:str)` / `http.redirect(status:int, location:str)` | 302        | 设置 `location` header                       |
| `http.statusText(code:int)`                                               | —          | 标准状态文字，例如 404 为 `"Not Found"`      |

状态码必须在 100–599 之间。重定向仅支持 301、302、303、307、308。`http.json` 支持[JSON 编码](/zh-cn/docs/stdlib/json/)可表示的数据，不返回 Result，无法用普通错误结果恢复不支持值的编码失败。helper 返回包含 `status`、`headers`、`body` 的完整响应对象。协议状态码和响应体中自行定义的业务字段 `code` 相互独立。

`http.status` 包含以下常量：

```text
ok=200, created=201, accepted=202, noContent=204
movedPermanently=301, found=302, seeOther=303, notModified=304
temporaryRedirect=307, permanentRedirect=308
badRequest=400, unauthorized=401, forbidden=403, notFound=404
methodNotAllowed=405, notAcceptable=406, requestTimeout=408
conflict=409, gone=410, contentTooLarge=413, uriTooLong=414
unsupportedMedia=415, rangeNotSatisfiable=416, unprocessable=422
tooManyRequests=429, headerTooLarge=431
internalError=500, notImplemented=501, badGateway=502
serviceUnavailable=503, gatewayTimeout=504
```

`http.code` 为旧的大写短别名对象，新代码推荐使用更清楚的 `http.status`。

## 服务容量与期限

在构造时传入选项，例如 `http.service({ max_body_bytes: 65536 })`；也可在绑定前修改已有的公开配置字段。数值选项都必须是正整数，0 不表示关闭限制。

| 选项                     |     默认值 |        最大值 | 限制对象                     |
| ------------------------ | ---------: | ------------: | ---------------------------- |
| `read_header_timeout_ms` |      5,000 |       300,000 | 接收请求 header              |
| `read_body_timeout_ms`   |     10,000 |       300,000 | 接收请求体                   |
| `write_timeout_ms`       |     10,000 |       300,000 | 写回响应                     |
| `idle_timeout_ms`        |      5,000 |       300,000 | 等待请求首字节，包括复用连接 |
| `handler_timeout_ms`     |     15,000 |       300,000 | handler 期限，起算点见下文   |
| `max_body_bytes`         |  1,000,000 |    16,777,216 | 请求体                       |
| `max_header_bytes`       |     16,384 |        65,536 | 请求 header                  |
| `max_connections`        |      1,024 |        32,768 | 在线连接                     |
| `max_active_requests`    |        256 |        16,384 | 活动请求槽位                 |
| `max_pending_requests`   |      1,024 |         8,192 | 等待中的请求                 |
| `max_buffer_bytes`       | 67,108,864 | 1,073,741,824 | 每个 listener 的受控传输缓冲 |

解释器从完整请求准备就绪时计算 `handler_timeout_ms`，等待执行也占用预算。native C 在派发前开始 handler 阶段计时，同时从首次观察到请求字节起保留原始请求总期限。环境变量 `KU_HTTP_REQUEST_TIMEOUT_MS` 配置 native 总预算（默认 30,000 ms，最大 300,000 ms），它不是 `http.service` 对象字段。async handler/PG 的有效期限不超过原始请求期限，不因准入、排队或取连接刷新。首字节观察时刻不等于数据包到达时刻，也不是客户端端到端时钟。

native runtime 的 handler worker 另外最多为 64 个。传输缓冲预算不是整个进程、用户堆、线程栈、内核 socket 缓冲或外部库内存的上限。

运行时对坏请求返回 400、缺失路由 404、路径存在但方法不匹配 405、请求体过大 413、请求目标超过 8,192 字节 414、header 过大 431、准入或缓冲额度耗尽 503、handler 期限到达 504。native 的期限检查是协作式的，在循环和 Ku 调用返回点检查，不能强行中断任意阻塞的外部调用。增加期限不能修复停顿的 handler。

HTTP/1.1 支持连接复用及按顺序返回流水线响应。请求包含 `Connection: close`、返回 4xx/5xx、协议出错或空闲期限到达时关闭连接。当前没有流式响应、SSE、WebSocket 或公开 `app.stop()` 方法。受监督 native 服务可通过 `ku service` 使用认证的健康/排空控制，与 listener 的 `close()` 不同。部署前请阅读[执行路径与已知问题](/zh-cn/docs/support/)。

## 只绑定，不启动

下列完整示例在解释器中申请空闲端口，随后立即释放，不开始处理请求：

```ku
import http from "std.http"

fn main(): null! {
    app = http.service()
    listener = app.bind("127.0.0.1:0")?
    println(listener.address)
    listener.close()?
    return ok(null)
}
```

listener 持有绑定的 socket，直到开始运行、显式关闭或最后一个持有它的值离开作用域。运行或关闭后不能重新启动。`close` 不是正在运行服务的并发停止句柄。

<span id="native-c"></span>

## 执行支持

`ku run` 和默认打包 runner 支持上面的完整服务端、客户端示例。同步 native C 支持 `app.listen(...)` 和 `text/html/json/empty/redirect` 响应。`http.json(value)`、`http.json(status, value)` 使用同步 JSON writer，设置 `application/json; charset=utf-8`，返回响应而非 Result；状态非法或编码不支持时 handler 失败（500），不是普通可恢复 Result。native C 不支持 `http.statusText`、模块级 HTTP 客户端及 `bind/run/close`。具名 async native handler 使用下文受限路径，不提供上面的完整接口；LLVM 不提供本 HTTP 库。

需要编译为 native C 时，保存下列服务为 `native-server.ku`。这个版本的 `/users/42` 返回纯文本 `42`：

```ku
import http from "std.http"

fn main(): null! {
    app = http.service()
    app.get("/health", fn() { return http.text("ok") })
    app.get("/users/{id}", fn(req) {
        return http.text(req.params.id.clone())
    })
    app.post("/echo", fn(req) { return http.text(req.body.clone()) })
    app.get("/empty", fn() { return http.empty() })
    app.get("/page", fn() { return http.html("<h1>Ku HTTP</h1>") })
    app.get("/go", fn() { return http.redirect("/health") })
    app.listen("127.0.0.1:8080")?
    return ok(null)
}
```

```sh
ku build --native native-server.ku -o native-server
./native-server
```

Windows 使用 `-o native-server.exe` 并执行 `.\native-server.exe`。可沿用前面的 curl 命令调用，结束时按 Ctrl+C。不要同时让两个示例占用 8080 端口。

## 具名 async native handler

以下独立示例使用具名 async handler。保存为 `async-server.ku`，检查后用 native C 后端编译，不使用解释器运行：

```ku
import http from "std.http"

fn main(): null! {
    async fn health(): HttpResponse! {
        return ok(http.text("ready"))
    }
    app = http.service({
        max_connections: 64,
        max_active_requests: 16,
        max_pending_requests: 32
    })
    app.get("/health", health)
    app.listen("127.0.0.1:8080")?
    return ok(null)
}
```

```sh
ku check async-server.ku
ku build --backend c --profile release async-server.ku -o async-server
./async-server
```

`curl http://127.0.0.1:8080/health` 预期返回 `ready`。运行其他同端口示例前先 Ctrl+C 停止当前程序；Windows 输出名使用 `.exe`。

`HttpRequest` 与 `HttpResponse` 注解适用于这条 native HTTP 路径。`HttpResponse!` 通过 Result 传播错误，运行时管理准入、handler Task 与响应生命周期。这不代表任意资源句柄、动态闭包或所有标准库操作都可放进 Task。解释器拒绝 async HTTP handler。这些 async handler 的响应构造限于 `text/html/empty/redirect`；只读 PG client 捕获的受限路径见[数据库示例](/zh-cn/docs/stdlib/database/#async-pg)。
