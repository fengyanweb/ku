# Ku 并发与 HTTP 压测

Ku 的用户级并发模型保持简单：业务代码通过 `async fn` 启动一次性 task，然后用 `await task` 或 `await task?` 等待结果。用户不能手动 `task.spawn`、`Task.new`、`runtime.schedule` 或 `thread.spawn`。

## task 规则

- `async fn` 调用会立即启动一个轻量 task。
- task 是句柄，不是线程。
- 普通 task 是 move-only，不能隐式复制，不能 clone。
- `await task` 会消费 task；普通 task 只能 await 一次。
- `await task?` 等价于 `(await task)?`。
- HTTP server 内部可以使用 task，但普通 handler 不需要管理 task。

## runtime 有界策略

当前解释器 runtime 的默认边界：

```txt
active task 上限: 1024
task queue 上限: 1024
blocking queue 上限: 1024
await 深度上限: 64
```

超过上限时，Ku 不会无限排队，也不会无限重试；超出的提交会结构化拒绝并进入 runtime 指标。

## 开发者 HTTP 压测 demo

启动 HTTP 服务：

```powershell
ku run examples\http_capacity_10m.ku
```

另开一个终端发起压测：

```powershell
powershell -ExecutionPolicy Bypass -File examples\http_bench.ps1 -Url http://127.0.0.1:8080/health -Requests 10000000 -Concurrency 1000 -TimeoutSeconds 600
```

这个 demo 是开发者视角：业务代码只写 `http.service()`、`app.get/post`、`fn()` / `fn(req)` 和 `return http.text/json(...)`。并发调度由 Ku runtime 处理，普通业务代码不导入 `std.task`，也不手动创建或调度 task。

压测输出应重点看这些字段：

- `Requests`：总请求数。
- `Concurrency`：客户端并发请求数。
- `WallMs` / `RPS`：总耗时和吞吐。
- `Errors`：网络错误或超时数。
- `LatencyP50Ms` / `LatencyP95Ms` / `LatencyP99Ms`：延迟分位。
- `StatusCounts`：HTTP 状态码分布，正常应主要是 `200`。

## 不能混淆的两件事

“千万请求压测”不等于“千万个活跃连接/协程同时常驻”。

10,000,000 个请求可以由一个或多个压测客户端分批并发发出；10,000,000 个真实 HTTP keep-alive 连接同时常驻则会受到操作系统 fd/端口、内核 socket buffer、内存、网卡、负载均衡、客户端压测机数量和超时策略限制。如果要验证千万级同时在线连接，需要单独的多机压测方案、内核参数、连接复用策略和服务端 runtime 配置。

仓库根目录的 `test.ku` / `run-test.ps1` 仍是 runtime 维护者使用的内部诊断入口，用来验证 active task 有界、超限结构化拒绝且不无限排队。它们不作为普通开发者业务示例。

## HTTP 并发边界

HTTP server 当前提供：

- `max_connections`：同时在线连接上限。
- `max_active_requests`：同时处理的请求/handler worker 上限。
- `max_pending_requests`：等待队列上限。
- `handler_timeout_ms`：handler 执行超时返回 504。
- `idle_timeout_ms`：连接首字节等待超时。
- header/body/write timeout：网络读写不无限等待。

普通 handler 用 Return 模型：

```ku
app.get("/health", fn() {
    return http.text("ok")
})

app.get("/user/{id}", fn(req) {
    return http.json({ code: 0, msg: "ok", data: { id: req.params.id } })
})
```

`fn(req, res)`、`res.write`、`res.end`、`reply.send` 不属于普通 handler 模型。
