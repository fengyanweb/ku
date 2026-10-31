# Native Task 与 HTTP 事件循环接口

本文件描述 `codex/multiloop-runtime` 的实际接线，基线为 v0.0.18-r1。它不改变旧版本发布范围，不代表解释器或其他后端已经采用相同线程模型。官网部署需要单独指令。

## 正常入口

```ku
import {http, time} from "std"

async fn hello(req: HttpRequest): HttpResponse! {
    time.sleep(10)?
    return ok(http.text(req.path.clone()))
}

fn main(): null! {
    app = http.server()
    app.get("/hello", hello)
    app.listen("127.0.0.1:8080")?
    return ok(null)
}
```

用现有 `ku build --native server.ku -o server` 构建。同步 `main` 只执行一次服务初始化；命名 `async fn` 编译为 Task 状态机，注册后由连接所属事件线程启动和推进。同一个应用可以继续注册同步 handler；同步 handler 在有界阻塞线程中运行。

支持零参数或一个 owned `HttpRequest` 参数，返回 `HttpResponse!`。`return ok(http.text(...))`、`?`、`try/catch` 沿用现有 Result 行为；链式调用不会自动解包。普通函数值别名可以用于注册。同步 setup 直接调用 async handler、通过别名调用，或把它交给同步函数执行，会在写出 native 产物前明确拒绝，不能执行签名占位函数。

请求提供 `method`、`path`、`body` 字符串，以及 `params`、`query`、`headers` 的点访问和字符串索引，例如 `req.params.id`、`req.query["q"]`、`req.headers[key]`。投影输出使用现有字符串所有权和预付分配检查。沿用当前 native map 规则：键不存在返回空字符串，query 值保留原有百分号编码；本次没有增加 URL 解码规则。响应构造接通 `http.text`、`http.html`、`http.empty`、`http.redirect`；可选状态参数使用现有整数表示。

原 native Task 的整数、布尔、空值、字符串、flat array、局部 bytes、函数身份、Task 调用/await、结构化 finally、time 和已支持文件操作仍沿用现有受限代码生成。这里没有把所有 Ku 对象类型、任意闭包捕获或所有标准库扩成新的异步后端；不支持的 native async 表达式应明确编译报错。

JSON 局部值可跨等待保存，支持解码、字符串键索引、子树移交和编码；所有权、预付预算及具体类型范围见 [Task JSON](native-task-json.md)（[English](native-task-json.en.md)）。Task 参数、捕获和公开返回值未因此扩为任意 JSON。

## 等待与固定归属

- HTTP 连接、parser、响应写出和该请求的 Task 固定归属一个事件线程。子 Task 继承归属，不偷取活动 Task。
- 同一 Task 只允许一个执行者；重复 wake 合并到其固定 ready 节点。driver 外部 pump 接受每轮最多 64 个 Task 量子，HTTP owner 每轮实际取 32 个；source frame 每量子最多 64 个 dispatch；真实 timer/await/I/O 等待仍登记后挂起。
- 纯循环 latch 和没有 Task 的空 scope 在有限 dispatch 量子内继续，不为每个整数迭代进行两次调度。取消仍在 dispatch 边界检查。
- 外部loop正在pump时不为每个自排队量子发送socket通知；pump末尾在同一发布锁下重查ready/期限，剩余ready返回deadline0，不再在driver锁内发送多余自唤醒。真正跨线程发布到已停泵的owner时仍发送持久唤醒，关闭和故障保留全组通知。
- POSIX HTTP owner 对已经 ready 的计算可合并即时 OS 探测：仅在没有紧急 HTTP 读写、Task 网络兴趣、待准入连接、已发布唤醒、关闭或故障时继续；新鲜单调时钟必须仍在首轮的同一毫秒，且最多连续 8 个原有 pump(32)，即 256 个逻辑 poll。每子轮仍检查原期限、清理及 group 状态；真实等待和任何 OS 探测前清空批次状态。Windows 保留原探测路径。该边界是合作式的，单次昂贵操作或 OS 停顿仍可能超界，不是严格 1ms 抢占。
- 动态 owned 槽位仍在返回、真实等待、分配预付、子 Task 转账、清理和结果取出时执行原计费检查。曾定向试验只对纯 Copy 预算量子跳过一次重复扫描；生成 C 证明条件生效，但同负载没有证明收益，因此已移除两个私有状态位及该分支，保留完整检查与 owned 回归。

`c_task_driver.rs` 的 external init/pump 不创建线程。实际线程由 HTTP loop 持有；没有创建成功的逻辑 owner 只能在无准入工作、请求关闭后由协调线程进行最后一次 pump，走正常 EXITED/join/destroy 路径。内部 raw C fixture 的旧共享队列模式仍保留，用于原 ABI 回归。

## TCP、DNS、TLS 与文件操作

| 调用 | 实际执行位置 | 期限与释放 |
| --- | --- | --- |
| plain TCP DNS | 共享有界 Task 阻塞池 | 入队前确定期限；`getaddrinfo` 返回后检查原期限，不能承诺中断其内部调用 |
| plain TCP connect | Task 的固定 owner poll/WSAPoll | 每次是非阻塞 connect/SO_ERROR，最多 64 个地址候选；不会在 DNS 线程等待 socket |
| plain TCP read/write | Task 的固定 owner poll/WSAPoll | partial read/write、would-block、EOF、半关闭及错误均保留；每次读或写推进最多 64 KiB，写缓冲跨等待保持拥有 |
| TLS client | 共享有界 Task 阻塞池 | 继续支持原配置和实现；没有宣称 TLS 握手及 TLS 读写已经全部异步化 |
| 文件/目录调用 | 共享有界 Task 阻塞池 | queued 可以摘除；已进入原生调用的记录和字节保持到物理完成 |
| PG/MySQL/Redis 同步 HTTP handler | HTTP 的有界阻塞线程 | 同一应用可混合同步数据库 handler 和 async timer/TCP handler；没有新增异步数据库协议实现 |

Task 阻塞池在整个 HTTP 服务中只创建一份，当前使用总 `KU_BLOCKING_THREADS` 中的一个线程，剩余额度交给同步 HTTP handler。纯 TCP 连接数不会增加线程数。独立的 async `main` 也在原有固定 Task 工作线程内进行 socket 事件等待，没有隐藏的每 Task 线程或额外 reactor 线程。

作业 readiness 用 `{slot,generation,owner}` 校验。旧事件、已经完成/取消的事件及别人的 owner 事件只会被忽略，不会关闭或推进复用后的 socket。排队取消在池锁内摘除固定环节点、释放输入快照，再由所属 Task 消费 DONE；正在原生调用中的工作不提前计为释放。网络读取超时会关闭失效连接；没有自动重试网络副作用或数据库操作。

## 资源配置与计量边界

`KU_EVENT_LOOPS` 为 1..32，`KU_BLOCKING_THREADS` 为 1..64，`KU_THREAD_BUDGET` 默认 16、最大 96；总额度包含 main、输出线程、事件线程和阻塞线程。默认事件数受实际 CPU/配额、总线程预算及 HTTP active/pending 额度共同限制。Linux 核对 affinity 和 cgroup v2 祖先 `cpu.max`，Windows 使用进程 affinity 和可取得的 Job CPU hard cap，macOS 使用在线 CPU 与明确线程预算。读取不到额外配额时不会声称已经检测到它。

HTTP 至少需要 main、输出线程、一个事件线程和一个同步 handler 线程；再使用 async 文件/网络阻塞设施时至少还要一个共享 Task 阻塞线程。显式配置不能超过总额度。`KU_THREAD_STACK_BYTES` 默认 2 MiB，要求 4096 字节倍数，范围 256 KiB..64 MiB；这是 OS 线程栈成本，不伪装为 Task 堆计量。

Task driver 最多 1024 个 resident slot，blocking job 也最多 1024 个。`KU_TASK_MAX_BYTES` 限制该 driver 账本中的固定结构、请求持有数据、预付快照、动态输出和仍需退役的实际作业，不是整个进程 RSS。HTTP 缓冲有进程共享的另一份预算；响应从 Task 交给 HTTP 时先在 HTTP 预算接纳，再退 Task receipt，避免交接期间脱离两者计量。

DNS/第三方库内部内存、OS socket 缓冲、线程栈和任意 Ku 堆不能据此宣称全部精确计量。整个执行进程的 OS 内存限制由 `supervise`/服务管理流程提供，平台条件和协调器自身的边界见版本及服务文档。没有精确分摊每个 Task 的所有第三方内存。

`/__ku/metrics` 的 Task 指标包括实际 ready/waiting/running/resident/timer、polls、ready 队列峰值等待时间；起点是 ready 发布，不是 HTTP 到达。Task blocking 指标区分 queued、running、resident、output charge 和已经取消但原生调用仍在运行的数量；`net_wait`/`net_step` 是 owner 中的非阻塞操作，不计作专属阻塞线程。

## 必要永久回归与结果入口

- `tests/native_task_source_test.rs`：固定 owner、公平 pump、wake 合并、关闭与从未绑定 owner 的退出；原 source 到 native 执行、字符串/数组、目录、bytes、TCP partial binary/EOF、旧 generation readiness。
- `tests/native_task_adapter_test.rs`：实际分配/所有权账本、子 Task transfer、finally/取消、原期限和 pool full；占住唯一原生 worker 时，排队任务取消仍必须移除并退役。
- `tests/native_http_test.rs`：真实 mixed async HTTP，request map 点/索引、owned 文件输出转 response、timer 超时后恢复、同步 handler 兼容、拒绝签名 stub 执行；两批 24 条 TCP 等待须全部发出 binary 输入后才回复，随后验证 read timeout、物理 close、重连和自然 drain。
- `tests/acceptance/runtime_matrix.py`：三档短负载、真实等待和资源回落入口；`tests/acceptance/multiloop_load.py`：固定30分钟正式入口。二者复用 `tests/fixtures/multiloop_service.ku`，吞吐、分位数、资源回收和平台 CI 以各自实际报告为准，本接口文档不能代替验收报告。

多循环不承诺线性加速。相同短循环负载的 owner/self-wake 改动前后对照已显示改进，N=4 仍可能低于 N=2；已捕获的共享 driver 元数据锁等待需如实说明，不能通过换负载、增加超时或改变账本放宽来隐藏。

本机必要回归使用 macOS arm64、Rust 1.89.0：16 条 source、12 条 adapter 和 3 条 mixed HTTP 回归曾分别通过，包含分配失败、owned 替换、子 Task 转账、取消和物理回收。删除尾部多余通知后，source 目标重新执行 16/16 通过，新增断言覆盖零自通知的持续推进、停泵后的外部定向通知和关闭广播。早期局部结果保留各自身份，最终三平台和正式窗口以冻结源对应的集中报告为准。

原生冻结源 `a31c113` 的三平台默认套件、Linux竞态/内存检测和新的正式30分钟已通过；其C/backend/IR与后续服务定向源一致。Rust协调器独立的RST退出修复由 `67d78d9` 三平台服务/真实PG检查通过。各次实际源、跳过项、旧失败和A19真实环境阻塞统一见[主验收记录](multiloop-runtime.md)，不将本接口文档当作平台验收证据。

参考部署的Windows原生代理更新/回滚和平台辅助线程预留由独立 `e40ab1b`、run39709674234验证；它不改变本文Task执行线程、I/O或编译输入。操作阶段期限、完整进程成本及外部演练条件见主验收记录。
