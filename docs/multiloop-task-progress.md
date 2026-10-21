# 多循环 Task 进度修复记录

核对日期：2026-10-21。本记录只解释本次多循环实现出现的新回退；历史七次 HTTP 504 仍是“历史出现过、本轮旧路径短测未复现、根因未确认、原正式 30 分钟验收尚未通过”，不能由下列新路径修复倒推其原因。

## 复现与实际原因

`tests/fixtures/multiloop_service.ku` 的 `/cpu` 在 async 函数中执行 5,000 次整数循环并断言结果；`/hot` 执行 200,000 次循环。原 5 秒、100 次到达/秒、N=1 的混合短测出现 179 个 504、201 个压测端主动限流，故没有继续反复运行 30 分钟测试。先隔离单请求和无业务流量，得到两条独立的源码证据：

1. HTTP 主协调器每轮无条件调用 `ku_http_group_notify()`，该函数同时通知各事件循环和协调器自己的 wake socket。主线程的下一次 `poll()` 因自己发出的通知立即返回，形成空闲忙转及广播。无业务请求、Task polls 始终为零时，约一秒实际消耗 1.30 秒进程 CPU。macOS 线程采样把主线程定位到 `ku_http_listen → ku_http_group_notify → ku_http_wake_notify → sendto`，事件循环反复排空通知。
2. async lowering 的整数 for、数组 for 和 while 回边会产生 `Suspend`。原 HTTP emitter 对每个回边立即返回 Pending；没有子 Task 的作用域完成也再次让出。5,000 次循环实际产生 10,002 次调度 poll。它们是人为公平边界，并非定时器或真实 I/O 等待。每次让出都引入重新排队、跨线程通知和驱动成本。

前者修复为只在启动完成、开始 drain、关闭 Task 等状态转换时广播；正常 Task 通知其归属循环，关闭期间才另通知协调器。后者在可保存状态的纯回边及空 Task 作用域内复用已有 64 次状态分发预算，预算耗尽才让出；真实 timer、I/O、子 Task 的完成确认以及清理期限继续使用原有等待/终结协议。没有提高请求期限、重做 M:N、改变 JSON 或启动暂停的全面分析。

## 固定条件与对照

- 同一台 macOS 26.5.2 / arm64，8 个物理和逻辑 CPU，8 GiB 内存；压测端同机 Python 3.9.6，无代理、TLS 和数据库。桌面及系统其他负载没有被隔离，因此数字只描述此短测。
- 工作树基于 `f0bb31f3420cde8e95ba3569f0fbd1af933c27bf` 加本次未提交改动；每轮先复制并固定 CLI，再由该副本执行 `ku build --native ... -o ...` 的默认构建。CLI 摘要是复现身份，不能冒充最终提交号。
- 相同 fixture、N=1、阻塞线程 5、总线程配置 16、Task 字节预算 16 MiB；独立 loopback 管理端口；业务 active 128、pending 128、connections 256、HTTP buffer 8 MiB。
- handler、header、body、write、idle 期限均为 1,000ms；`/timer` 等待 25ms。单请求顺序 `/ok → /timer → /cpu → /hot → /cpu → /timer → /ok`，每次连接关闭；各次请求前后读同一组 Task 指标。
- CPU 来自 `ps -o time=` 的进程累计 CPU 时间差，分辨率约 0.01 秒；0.00 只表示该分辨率下未见增长。poll 计数含该 Task 的执行与终结步骤。

| 条件 | 修复前 | 两项修复集成后 |
| --- | --- | --- |
| 无业务约 1 秒 | 1.002 秒墙钟 / CPU +1.30 秒，0 polls | 1.010 秒墙钟 / CPU +0.00 秒，0 polls |
| `/cpu` 5,000 次 | 100.95–105.20ms；每次 10,002 polls；200 | 1.83–3.59ms；每次 314 polls；200 |
| `/hot` 200,000 次 | 999.99ms；100,769 polls 后 504 | 68.68ms；12,502 polls；200 |
| `/timer` 25ms | 25.14–25.32ms；每次 3 polls；200 | 25.80–27.00ms；每次 3 polls；200 |
| 收尾 | 请求完成后 Task/ready/waiting/timer 为零；进程退出 0 | 请求完成后 Task/ready/waiting/timer 为零；进程退出 0 |

修复前 CLI SHA-256：`259d20bf8871734b97ee1d2a6153a0d0bd261f982c14f27881880edfe812c454`。

集成后 CLI SHA-256：`503681ffa0efb57e4d3ba6de5c6a8a5abd8910f7fc61a45cb172374c7680ca12`。

这是两项修复集成前后的对照，不把耗时改善分别归因成每项各自的独立收益。5,000 次循环结果断言正确、完成后计数归零，支持该样本持续推进；不据此声称所有状态机均无问题。新路径的正式混合验收、三平台 CI 与其他故障矩阵仍按主验收报告分别列结果，此记录不替代它们。

## 保留证据与清理

精简原始摘要保留在本机 `Downloads/Ku_多循环验收_2026-10-21/`：`short-load.json`、`task-progress-single.json`、`task-progress-idle.json`、`task-progress-idle-stack.txt`、`task-progress-single-fixed.json`、`task-progress-idle-fixed.json`。后两份没有覆盖失败样本。证据目录是本机交付资料；永久复现使用仓库中的 fixture、`multiloop_load.py` 以及相关回归，CI 产物独立归档。

本轮临时 CLI 副本、native 产物、地址文件、运行日志及验证进程都在私有系统临时目录创建并在 `finally` 中清理，未向项目添加一次性探针脚本。

## 多循环自通知成本的第二次定向修复

在上述两项修复后，固定 8 个压测线程、每请求 5,000 次整数循环的短测仍显示 N=4 比 N=2 慢。另取 5 秒 N=4 样本，1,816 次请求全部正确，服务 CPU 增量 8.50 秒，Python 发生器 CPU 0.539 秒；因此该样本不能归因为 Python 已占满单核。1 秒线程采样中，一个事件线程的 389 个样本有 347 个位于 Task pump，其中 160 个位于 `reconcile_charge → driver mutex wait`、58 个位于 `run_one` 返回后的 driver 加锁、43 个位于 `enqueue_local → signal_owner → HTTP event_wake → sendto`。这些是包含等待时间的线程采样数，不是 CPU 百分比。

实际原因之一是在归属线程正在 pump 时，每个公平量子重新入队仍持全局 driver 锁通知这个正在运行的线程，额外取得 HTTP group 锁并发送 UDP 自唤醒。修复保留同一 64-dispatch 预算，只在 external owner 正在 pump 时合并该通知；pump 返回后保留就绪状态/立即再推进，外部完成、取消和关闭继续发通知。

相同 debug native 构建、相同固定 8 个客户端与 5 秒窗口下，独立短矩阵得到：

| 事件循环 | 修复前请求/秒 | 修复后请求/秒 | 修复后 p99 |
| --- | --- | --- | --- |
| 1 | 549.44 | 1,969.97 | 5.7ms |
| 2 | 660.50 | 2,496.15 | 4.4ms |
| 4 | 398.08 | 2,028.79 | 5.7ms |

前 CLI 为 `4dfa8b04db2c54220b6dcf4d4c4422902378ea6aca30a823c67e769ad79c10d4`，后 CLI 为 `0923e8db53dfb30ce689c91157edf54418f27d82b0052ea03a99016b1f4c3674`。原始 N=4 栈和摘要为本机证据目录的 `N4-CPU5000-driver-wake-stack.txt`、`N4-CPU5000-driver-wake.json`；同配置 1/2/4 前后矩阵已精简归档至 [runtime-matrix-macos.json](evidence/runtime-matrix-macos.json)。

后续 release native 构建的完整**本机短矩阵**分别测得 4,177.73 / 3,982.90 / 2,177.21 请求/秒，该短矩阵列出的场景通过，不代表 A01—A18 及三平台、正式窗口全部完成。该组改变了构建 profile，不能当成上述通知修复的独立收益。N=4 在此短循环样本仍低于 N=1/2；N=1/2 的同机 Python 发生器约占用 4.3 秒 CPU/5 秒窗口，N=4 的服务端每请求 CPU 成本亦较高。不宣称线性扩展，也不把仍存在的共享管理成本包装成多核性能已全面收口；正式验收及剩余限制以[主验收表](multiloop-runtime.md)为准。

## 无实测收益的局部优化已撤回

曾尝试在 async frame 增加 dirty/copy-quantum 标记，跳过纯 Copy 量子的拥有字节重算。对应 release CPU5000 短测为 N=1/2/4 约 3085/3086/2085 请求/秒，没有证明端到端收益，因此定向撤回这两个标记及分支，不继续为选型追加优化。owned 赋值/子 Task 取消的必要永久回归保留。

当前保留的是已有直接证据的固定 owner 定向 wake、pump 期间自通知合并、64-dispatch 协作预算、真实 TCP readiness/取消及正常归还账本。撤回不触碰通用 Value 复制/释放与 AST 两项全面分析，也没有改变 JSON 默认实现。

## 集成时发现的部分启动退出问题

独立复核对 N=1/2/4 的事件 owner 初始化分别注入 `calloc`、wake channel、queue 失败。原路径中已经创建但初始化失败的线程先退出，留下它名下的 Task 退休记录无人推进，可能把明确的启动错误转成 75/80。调用链是 HTTP owner 初始化 → wrapper 退出 → 主线程等待 Task 退休，并非历史请求停顿样本。

最小修复在原 owner 的 OS 线程内标记失败/drain，触发幂等 close，继续绑定并推进本 owner 的正常退休流程，最后发布 exited；未启动 owner 仍由原有关闭路径处理。没有外部迁移正在执行的 Task、强行释放或提高期限。永久入口 [`native_http_startup_test.rs`](../tests/native_http_startup_test.rs) 的九项注入在本机通过，具体 listen 错误退出 1，正常 fixed-loop drain 同样通过；最终提交三平台结果仍待 CI。

## PostgreSQL 短性能的证据边界

早期经 Python 故障代理的 N=2 五秒性能轮次有 49 次 `JSONDecodeError`，当时没有保留响应 body。它们不能被补写为 acquire timeout、错误 JSON 或数据损坏；两次同参数有界诊断分别 10914/10914、11537/11537 成功，也不能倒推其根因或修复。没有复现证据支持修改生产 PG 池，因此未为此修改连接池或重放 SQL。

诊断驱动现保留最多 12 个错误例子，区分字面 `error:<code>`、JSON 解析错误、字段不符、HTTP 状态和连接错误。性能模式去掉 Python 故障代理，使用直连私有 PG；第一次直连每请求新建连接在 16265 次成功后得到 1365 次压测端 `OSError errno 49`，尚未取得任何 HTTP body。其范围与 macOS 49152—65535 的 16384 个临时端口吻合，修正为固定 8 个持久 HTTP 客户端；失败请求仍计一次，不重发。生产 Ku 架构、PG 池、SQL 和期限未变，正常故障模式仍保留代理。

最终直连短矩阵 N=1/2/4 分别 29048/28559/27274 次成功、错误 0、5808.41/5709.50/5450.24 请求/秒，p99 4.2/4.1/4.4ms。固定共享 PG 连接 4，收尾 pending/bytes 为零，正常退出后 PG 连接为零。各档每个 owner 都有实际 HTTP 请求量；没有线性扩展结论，也不能将代理/直连、每次建连/持久连接的不同拓扑当成 Ku 前后加速比。摘要和旧失败分别见 [PG 性能](evidence/pg-performance-macos.json)、[有界诊断](evidence/pg-performance-diagnostic-macos.json)。

## 本轮最终收口位置

[`multiloop-runtime.md`](multiloop-runtime.md) 集中列 W1—W7、A01—A19、三平台能力差异、实际使用入口、1/2/4 成本和冻结的正式 30 分钟参数。已有本机短结果与服务恢复有独立产物摘要；最终提交 CI、正式窗口和最终清理必须在实际执行后补结果，不能用本记录提前宣布通过。没有授权双宿主/入口/数据库故障域环境时，A19 真实演练保持未执行。

最新 `final-preflight.json` 的 250 个请求中出现 2 个 `/hot` 504，短预检未通过，正式 30 分钟尚未启动。这个新样本保留并做有界诊断，根因尚未确认；不能把旧短矩阵的通过或上述修复替代最新失败，也不能把它补写成历史七次 504 的原因。

本轮一次性验证均在系统临时目录执行并清理；必要永久回归、可复现驱动、配置和精简失败证据保留。当前仍在用的冻结 CLI 副本应在最终验收后由主执行者清理，不能在仍被验收使用时删除，也不能提前写成所有交付进程/产物已清理。
