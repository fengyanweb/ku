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

解释器与各 native 切片的执行证据见实施记录；不同切片的测试结果不能互相替代。
v0.0.18 开发分支已接通 native C 的单 worker 有限源码子集：AST 经独立 Task IR
生成 Start、Move、Await、函数退出与 If 分支的正常 scope drain，不嵌入解释器或 runner 源码。
既有检查点证据与本次源码 If、Owned/Pending 专项分开记录；专项不能替代本次完整
workspace/native 全集或精确提交三系统 CI/sanitizer 验收，不是正式发布。
实际结果与历史失败/修复的分开证据见 [工作日志](v0.0.18-worklog.md)。
结果等待与 ACK 等待复用固定槽位，不按每次等待分配；函数退出先移交全部 sibling，
再等待逻辑清理 ACK。迟到 observer 可以保留控制存储，但不能保留已丢弃的 payload。
正常 scope 超期是外层运行时 `task/shutdown_timeout`，不同于业务 Result.err，
也不会把正常父任务标成 Cancelled；已有取消原因不变，绝对清理期限只收紧、不续期。
LLVM、`ku ir` 和 `--emit-ir` 的同步 IR 路径仍拒绝 async。
已经进入系统或外部库的阻塞操作仍不能硬杀，迟到结果只能清理，不得恢复已取消任务。

### 当前 native C 源码子集

import 展开后只能有顶层、非泛型 async 函数；入口必须是无参数的
`async fn main(): null!`。函数参数为 `int/bool/null/str` 或对应单层 Result，
返回类型必须显式为 primitive `T!`。函数体支持局部绑定、源码 `if` / `else` 及分支词法作用域、直接 async 调用、
Task move、Await、`ok`、`?`、primitive print/println、显式 return，以及字符串常量 fail。
Copy 表达式支持 int 的一元 `-`、`+ - * / %`、`== != < <= > >=`，以及 bool 的
`!`、`== !=`、`&& ||`；不做 bool/int 隐式转换。整数运算先检查边界，溢出和除/余零
分别报告 `integer overflow`、`division by zero`；`MIN / -1` 与 `MIN % -1` 均是溢出。
这些是外层 runtime failure，不是普通 Result.err：即使写 `result = await child`
而不写 `?`，也不能收到该错误后继续业务语句；仍经原有 owner/child 清理链退出。

普通二元式先完整求值并保存左值，再求右值；左 Copy 值在右侧 Await 挂起期间保活。
`&& ||` 只执行必要的右侧表达式，未选中的 Start、实参 move、Await、Print 和 `?`
都不执行，但右侧仍受静态类型与预算检查。若 Task 在逻辑表达式前已经创建，短路
跳过它的 await 不免除当前 scope 的最终清理责任。
新字符串表达式目前仅支持静态字面量；内部 ABI 仍负责 owned 参数/结果的 move/drop。
接纳/OOM 拒绝仍消费源码已经移动的实参，返回可 await 的失败 Task，错误不再申请内存。

If 包括嵌套 `else if`，条件必须是 bool，可使用当前子集的 Await、`?` 和短路表达式。
只执行选中的分支，但两臂都必须通过静态类型、所有权和资源预算检查。非空分支正常
结束时，先移交并等待它仍拥有的 Task 的真实清理 ACK，再释放本臂局部 Owned Value，
之后才能进入汇合点。return/fail/`?` 错误退出复用最终退出清理，不执行另一臂或汇合点。
内层可以用显式类型声明 shadow 外层同名局部；初始化表达式先读取原环境，离开分支
后恢复外层名字。普通重复赋值仍不支持。分支可以直接 await 尚可用的祖先 Task，
但不能将 Task move 到另一个词法作用域；只有每条仍会到达汇合点的路径都保有同一
Task 时，才能在汇合后 await 它。臂内局部不能在臂外访问。
ScopeEnter 本身不启动期限；空/inline failed 集合不创建不存在的清理 D。

```ku
async fn Child(value: int): int! { return ok(value) }
async fn main(): null! {
    first = Child(3)
    second = Child(4)
    moved = first
    a = (await moved)?
    b = (await second)?
    println(a)
    println(b)
    return ok(null)
}
```

仍拒绝循环/递归、重复赋值、跨词法作用域 Task move、try/catch/finally、闭包/函数值、
同步用户函数调用、借用 async 参数、Task 参数/返回/容器/clone、未绑定的 Task 临时、
float/混合类型算术、str/null/Result/Task 比较、动态堆表达式，以及异步标准库 I/O。
未支持形式在生成 artifact 前明确报错。
用户 cleanup 仍不能 Await；内部 ACK continuation 不是新的用户语法。
该子集只使用一个真实 OS worker 和条件等待，不递归 poll child。
root 使用最多 1024 个固定驻留槽；字节接纳按固定存储和生成 instance 大小计费，
不是操作系统 RSS 限制。编译器的函数/槽/操作硬限也不限制程序累计执行时间：
无递归调用图仍可产生大量顺序工作。普通计算等待不擅自增加全局超时。
表达式没有提高原有 64 函数、每函数 64 槽/256 状态、全程序 4096 操作及
1,000,000 字面量字节/分析工作量上限。Parser/Checker 的语句体嵌套限制为32层；Task
lower 对已构造 AST 的 If 另限32层，表达式仍受 `depth > 64` 拒绝与共同资源预算约束。
这些是分别拒绝的边界，不是复杂源码必定能达到32层或源码可写64层的承诺。
print/println 目前仍调用同步 stdio；阻塞输出不是已接入 netpoll 或 blocking pool 的 I/O。
M:N、netpoll、事件驱动 HTTP、native blocking pool、完整 RSS 预算及性能/soak 尚未完成；
不能据此承诺 CPU 并行或高并发吞吐。

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

`&` 保证不消费句柄及不通过 borrowed 根直接写透明值，不表示函数没有 I/O 或 opaque client 内部状态变化。解释器对 borrowed 读取还检查调用所在线程和 task，跨线程 / task 使用会被拒绝。它不增加用户线程、spawn、detach 或手动调度 API，Task 仍为 move-only，`await` 仍消费一次。上述同步调用/函数值示例不属于首片 native async 子集，native 构建仍会拒绝。

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
