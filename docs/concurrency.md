# Ku 并发与 HTTP 压测

Ku 的用户级并发模型保持简单：业务代码通过 `async fn` 启动一次性 task，然后用 `await task` 或 `await task?` 等待结果。用户不能手动 `task.spawn`、`Task.new`、`runtime.schedule` 或 `thread.spawn`。

## task 规则

- `async fn` 调用会立即启动一个轻量 task。
- task 是句柄，不是线程。
- 普通 task 是 move-only，不能隐式复制，不能 clone。
- `await task` 会消费 task；普通 task 只能 await 一次。
- `await task?` 等价于 `(await task)?`。
- HTTP server 内部可以使用 task，但普通 handler 不需要管理 task。

## Task 作用域与取消合同

v0.0.18 第二阶段已采用以下规则；这不表示所有后端已实现。完整合同见
[Task 所有权、取消与清理预算](semantics.md#task-所有权取消与清理预算)。

- 当前持有 Task 句柄的所有权作用域负责清理；move 会转移责任，不永久绑定创建位置。
  作用域正常结束、return、错误传播、panic、超时或取消时，取消仍持有的未完成 Task，释放已完成但未 await 的 payload。
- 兄弟失败本身不取消其他兄弟。父任务处理错误后可继续；只有父作用域因错误传播退出时，才清理它仍持有的兄弟任务。
- cancel/timeout 是不可由普通 catch 捕获的内部终止，与完成竞争唯一终态。
  finally 的 return、fail、panic 和迟到成功均不能覆盖已经获准的取消/超时。
- 一次根取消只有默认总计一秒的单调时钟绝对预算，所有子任务、嵌套 finally 和 drop 共用，
  外层 shutdown 剩余预算更短则从短；不能按任务逐个续期。超时记录未完成清理，不能冒充成功。
- 取消展开先请求 owned 子任务取消，再由内向外执行 finally，随后 drop 本作用域局部。
  清理期间禁止新建 Task、await 或提交新的 sleep/timer、网络等待和 blocking job；同步 close/drop 与有限计算仍受预算约束。

本轮解释器生命周期切片已通过本机全量回归，具体证据与未覆盖边界见实施记录；native async、stackless frame、M:N、netpoll 和事件驱动 HTTP 尚未实现。
native C / LLVM 继续明确拒绝 async lowering，不以同步代码或解释器回退冒充支持。
已经进入系统或外部库的阻塞操作仍不能硬杀，迟到结果只能清理，不得恢复已取消任务。

## 同步只读借用与 async

`&name: T` 的借用期只覆盖当前同步调用。第一版 `async fn` 不能直接声明 borrowed 参数，checker 返回 E0913；不会根据参数是否出现在第一个 `await` 之前放宽规则。借用值也不能进入闭包捕获或 task frame。

async 函数可以拥有普通参数，并在函数内部调用同步借用函数。例如解释器可执行：

```ku
fn Count(&text: str): int { return text.len() }

async fn CountLater(text: str): int! {
    return ok(Count(text))
}
```

同步调用结束后借用即结束；后续 `await` 不会携带这份借用。async 函数也可以拥有 `fn(&str): int` 类型的同步 callback 值，这与 async 函数自身声明 `&` 参数不同。callback 的捕获与同次调用的重叠仍接受普通借用冲突检查。

`&` 保证不消费句柄及不通过 borrowed 根直接写透明值，不表示函数没有 I/O 或 opaque client 内部状态变化。解释器对 borrowed 读取还检查调用所在线程和 task，跨线程 / task 使用会被拒绝。它不增加用户线程、spawn、detach 或手动调度 API，Task 仍为 move-only，`await` 仍消费一次。native async ABI 的既有不支持边界保持不变。

## runtime 有界策略

当前解释器 runtime 的默认边界：

```txt
active task 上限: 1024
task queue 上限: 1024
blocking queue 上限: 1024
await 深度上限: 64
```

超过上限时，Ku 不会无限排队，也不会无限重试；超出的提交会结构化拒绝并进入 runtime 指标。
空闲 task/blocking worker 阻塞等待有界队列并由新任务唤醒，不做固定间隔轮询；单 worker 内部仍可在 `await` 时执行一个已排队子任务，避免嵌套等待饥饿。这些都是 runtime 内部行为，不增加用户级并发 API。
但当前 await、blocking completion 等待和 shutdown 路径仍含短间隔检查；取消清理期间不再帮跑用户任务。
固定 worker 和有界队列不等于已经实现可挂起的 stackless M:N 调度或事件驱动连接管理。

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
