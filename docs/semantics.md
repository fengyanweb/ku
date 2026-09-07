# Ku 语义合同

这里固定跨 checker、解释器、IR 和 native 的语义不变量；源码拼写与 API 细节的规范性入口是
[syntax.md](syntax.md)。[诊断合同](diagnostics.md) 固定 CLI/编辑器字段与错误码。
历史版本文档只说明那个版本，不是引入旧语法或兼容别名的理由。

实现现状、明确拒绝的组合和待验收工作单独记录在 [v0.0.18 实施记录](v0.0.18-worklog.md)、
[自举状态](self-hosting.md) 和 [协议状态](protocol-foundation.md)。
规范要求某行为，不表示所有后端已经实现；缺失路径必须明确拒绝或记录为缺陷，不能静默回退解释器、隐式 clone 或声称完成。

## 值和调用

- Owned 默认 move；独立副本显式 `.clone()`；作用域退出确定性 drop，不引入全局 tracing GC。
- `str`、array、object、struct、enum、函数值及其拥有的 payload 遵守已有 ownership 规则。
  泛型不能把具体 Owned 实参当作 Copy，必须用具体类型验证函数体。
- `.clone()` 不是所有类型都有的能力；Task 和 move-only native handle 禁止 clone。
- 函数值调用不消费函数值；不能直接 move 出 shared closure 中的 Owned 捕获，需显式 clone。
- 参数先按源码顺序求值一次，再按参数模式传递；优化不能重复求值或让临时 owner 提前 drop。
  同调用内 borrow 与 move/mutation 冲突由 checker 拒绝，不靠参数重排绕过。
- 赋值覆盖的右侧先物化；仍参与右侧求值的旧 owner 不能提前释放。
  需要保留来源的 callback/投影必须继续遵守已有快照及来源分析，不能通过泛型或 native 优化擦除。

## 同步只读借用

声明只写 `&name: T`，调用仍写 `Read(value)`；函数类型保留 `fn(&T): R` 的槽位模式。
borrowed 参数非 owning、只读、同步调用期有效；callee 不 drop 来源。
来源及必要的临时 owner 必须活到调用结束，后续实参求值经 `?` 中止时也必须清理。

不引入 `view/ref/borrow` 别名、`&mut`、first-class `&T`、生命周期语法、借用返回、
借用存储、闭包捕获借用、async borrowed 参数或跨 await 借用。
已明确拒绝的首版组合以实现状态为准；禁止用隐式 clone 掩盖这些边界。

## 错误与清理

recoverable `Result` 的 ok/err payload 都参与 move/drop；错误字段为 `domain/code/message`。
`?`、fail、catch、finally 与 return 的清理不能重复释放或漏掉部分初始化的 owner。
catch 处理 recoverable failure，不代表任意 panic、进程级 OOM 或 runtime invariant 都可恢复。

保留既有直接索引的 fatal 边界以及对应 safe Result API；不能把直接索引偷偷改成第二套行为。
反过来，未处理的实现 `exit` 也不能事后称为设计好的 fatal 语义。
发送后不确定的数据库错误不能被自动重试，无法完整证明任意 SQL 的 session purity。

HTTP timeout 的 native 既有合同是不可恢复的请求超时，超时展开中的 finally
共用固定一秒清理窗口。若首次超时发生在普通 finally 内，不恢复该块的剩余语句；
这不是中断阻塞系统调用或 FFI 的线程抢占；解释器必须与其对齐。

## Task 所有权、取消与清理预算

以下是 v0.0.18 第二阶段已采用的语义决策，不再是待决问题；本轮解释器生命周期切片已通过本机全量回归，具体证据与未覆盖边界见实施记录。
native async、stackless Task frame、M:N 调度、netpoll 与事件驱动 HTTP 尚未实现，
不能把这份合同或既有 native HTTP timeout 当成 native Task 取消已经完成的证明。

- 清理责任属于当前真正持有 move-only Task 句柄的所有权作用域；合法 move 同时转移责任，
  moved-from 位置不得再次取消、等待或 drop。作用域因正常结束、return、错误传播、panic、
  超时或取消而退出时，请求取消仍持有的未完成 Task；已完成但未 await 的结果或错误 payload 确定性释放。
  只读闭包捕获和同步 `&` 借用不转移这份责任，callee 的 borrowed 参数不是 owner。
  原 owner 退出后，只读捕获可以保留 Task 标识的控制块，但不能保留或重新取得已经释放的 payload；
  这不增加用户级状态查询、cancel 或重复 await API。
- 兄弟任务失败不直接取消其他兄弟，也不直接改变父任务状态。父任务 await 后处理错误并继续时，
  兄弟继续运行；若错误传播导致父作用域退出，才按作用域退出规则取消它仍持有的其他任务。
- 取消和超时是内部控制终止，不是普通 Result/Error，不能被 catch 捕获。
  completion 与 cancel/timeout 在同一裁决点竞争唯一不可逆终态：已提交完成不能追溯取消；
  取消先获准后，迟到成功或错误只能安全 drop，不能再作为成功结果返回。
- 每次根取消以单调时钟建立默认总计一秒的绝对清理截止时间。
  嵌套 finally、子任务、frame 和作用域清理共享该截止时间，不按层级或任务重置；
  外层 shutdown 剩余预算更短时取较小值。预算耗尽保留原取消/超时原因并记录清理超时和未完成数量，
  不得返回成功；main/runtime 关闭仍保留既有 `task/shutdown_timeout` 边界。
  普通作用域退出的子任务清理超期也报告 `task/shutdown_timeout`，不能仅因清理子任务而把正常父任务标成 Cancelled。
- 先请求取消当前 frame 仍持有的未完成子任务，再由内向外展开 finally，最后释放各作用域的局部 owner。
  finally 可访问尚未释放的局部值，但受同一预算约束；清理中的 return、fail、panic 或其他错误不能覆盖主终止原因，
  只能记录为被压制的清理结果，并在剩余预算内继续外层清理。
- 取消清理期间禁止新建 Task、await、新提交 sleep/timer、网络等待或 blocking job。
  同步 close/drop、必要的 runtime 注销与有限同步计算仍允许；safepoint 在预算耗尽后终止继续执行。
  已进入系统或外部库的阻塞操作不能被硬杀，只能受有界 blocking pool 隔离；迟到结果清理后不得恢复用户任务。

本合同不增加用户级 cancel、detach、spawn、调度入口或新的 Ku 语法；逐项实现和验收状态见
[v0.0.18 实施记录](v0.0.18-worklog.md)。

## 并发和协议主路径

- 用户只调用既有 async 函数并 await；Task move-only，await 消费一次。
  不公开 `task.spawn`、`Task.new`、`runtime.schedule`、thread 或 detach 第二入口。
- HTTP 路由参数继续 `/user/{id}`，配置继续 snake_case（如 `read_header_timeout_ms`、
  `max_active_requests`），删除路由 API 只用 `del`。
- 普通 HTTP handler 只允许 `fn()` / `fn(req)` 并返回 response；不接受 `fn(req, res)`。
- 数据库业务主路径只用 `module.client(config)?`，内部自动有界池化；不恢复 raw/pool 兼容入口。
- 等待任务/连接数、队列数与实际 retained bytes 都需要有限预算；有固定 worker
  不等于已经实现 stackless async、事件驱动 I/O 或证明高并发。

目标仅为 Windows x64、Linux x64、macOS arm64 的分别构建与验收；不存在三系统通用单一二进制。
十万挂起任务、万级同时 keep-alive、固定环境 Go 对照和一小时 soak 必须有绑定提交的实际结果，
总请求数、假数据库、链接成功或 CI 配置存在均不能替代对应验收。
