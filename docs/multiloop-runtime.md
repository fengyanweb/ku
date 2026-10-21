# 多线程事件循环实施与验收记录

核对日期：2026-10-21。状态：实现已接通，本机短矩阵及服务恢复已有结果；最后短预检仍有失败，最终提交的三平台 CI、正式 30 分钟综合验收尚未通过，不能宣布整套任务完成。基线为 v0.0.18-r1（`f0bb31f3420cde8e95ba3569f0fbd1af933c27bf`），分支为 `codex/multiloop-runtime`。本分支不回写 v0.0.18 标签，不上传或部署官网。

下列证据来自注明 CLI/产物摘要的工作树构建，并非最终提交的认证。最终提交号、CI 链接和正式负载结果应随实际执行更新本文；不能用早期短测或跳过的 CI 步骤替代。

## 当前调用链与方案

- `src/ir/task{,_lower}.rs` 保存 async 状态；`src/backend/c_task*.rs` 输出 Task 状态机、driver、timer、取消与有界阻塞设施。原 driver 有本地 FIFO 与偷取，网络通过阻塞设施；它与 HTTP 事件循环分开。
- `src/backend/c.rs::emit_http_runtime` 输出 HTTP；原样提取为 `src/backend/c_http_runtime.c`，仍由 include_str 嵌入独立产物，没有运行时源码依赖。原来一个 poll/WSAPoll owner，最多 64 个同步 handler 线程。
- PG 使用 libpq 非阻塞接口但在调用线程同步推进；MySQL/Redis 为同步调用。HTTP handler 的有界后台执行是这些适配的运行模式，不宣称数据库全异步。
- `src/process_execution{,/platform}.rs` 是独立程序的 OS 限制和批次协调器，不是常驻 HTTP 实例的重启负责人。

选择复用已有 poll/WSAPoll readiness 操作和解析器，补固定归属循环、总额度、可靠通知；Windows 没有 overlapped operation，不借用 IOCP 的完成寿命承诺。引入 libuv 需要新增 SDK、打包依赖且仍需实现 Ku 自身状态机，因此优先复用已有平台边界。

服务初始化仅一次；route 表发布后只读。事件线程不执行同步 handler；整个进程的事件线程、后台线程、连接和缓冲限额统一。Task 固定归属、不依赖偷取。常驻进程由一个 `ku service` 重启负责人管理，原 `supervise` 的限制不降级。

## 保持的决定

JSON 使用旧默认；通用 Value 复制/释放与 AST 全面分析继续暂停，仅修本次实际调用路径。数据库不自动重试，失败确认丢失不得宣称回滚。历史 504 记录保持证据不足，不追加追溯。v0.0.18 发布不变。

## 正常入口与线程归属

```sh
ku build --backend c --profile release service.ku -o service
KU_EVENT_LOOPS=4 KU_BLOCKING_THREADS=5 KU_THREAD_BUDGET=16 ./service
```

`service.ku` 是用户自己的现有 HTTP 程序。仓库内可运行的普通服务示例为 [`deploy/service/http_service.ku`](../deploy/service/http_service.ku)，按[部署说明](../deploy/service/README.md)在每个 slot 同目录准备实际 `http.addr` 再构建、运行。混合 async/CPU/timer 示例为 [`tests/fixtures/multiloop_service.ku`](../tests/fixtures/multiloop_service.ku)，验收驱动在私有临时目录写入它需要的地址文件。不要直接从仓库根目录运行后误把缺少地址文件当成服务启动失败。

真实数据库与错误处理示例为 [`tests/fixtures/http_pg_lifecycle.ku`](../tests/fixtures/http_pg_lifecycle.ku)：初始化一次 `pg.client(config)?`，handler 捕获 client，使用参数化 `query`、逐字段读取、`try/catch` 返回既有错误码。由 `http_pg_lifecycle.py` 创建独有临时表和连接文件后运行；其中写入/故障路由属于隔离验收，不应直接用于公网业务 API。

同步 `main` 只初始化一次。HTTP 的同步 handler 在固定后台线程执行；具名 `async fn handler(): HttpResponse!` 或一个 owned `HttpRequest` 参数由同一组 HTTP 事件线程推进。请求的 method/path/body、params/query/headers 沿用现有字段模型。异步函数仅在路由注册处接入，不能在同步代码中通过别名逃逸后当普通同步函数执行；不支持的用法在产物生成前报错。解释器、LLVM 等入口保留原支持范围，不宣称新增这些后端的 native 多循环能力。

| 状态/操作 | 归属与推进 |
|---|---|
| 接入 | 一个共享非阻塞监听 socket；短锁与轮转令牌指定唯一接受者，接受后的连接固定归属，不迁移 |
| HTTP parser、读写、输出 | 原连接所属循环；readiness 通过 poll/WSAPoll 返回该拥有者 |
| Task | 固定 owner，子 Task 继承；有限 ready 批次，每个协作量子最多 64 个 IR dispatch；不在任意原生指令中抢占 |
| timer | 单调绝对期限；等待不占阻塞线程，ready/timeout/取消经过同一状态机 |
| 普通 TCP client | DNS 在共享有界设施；连接、部分读写、EOF 在 owner readiness 上推进，代次阻止旧事件作用到新记录 |
| 文件、TLS client、原生库 | 既有同步操作在有界后台设施，不能冒称其内部全部异步；实际返回前保留数据、句柄和槽位 |
| 数据库 | 同步 HTTP handler 的后台线程持有独占 lease；详见[数据库与输出](database-runtime.md) |
| 常驻恢复 | 独立 `ku service` 管理整个服务进程，详见[参考部署](../deploy/service/README.md)；不把 Task 超时当作线程已经停止 |

Task 返回 Result 的既有成功/错误行为不变；没有自动解包、隐式 clone、用户 spawn API 或新的借用类型。输入转交与响应交回都在前一账本退款前取得下一责任人的配额。取消尚未进入原生调用的作业会摘队并释放拥有数据；已执行者继续占用至实际返回，超过清理期限则退役整个实例。

## 配置与总量

| 配置 | 默认/上限 | 实际含义 |
|---|---|---|
| `KU_EVENT_LOOPS` | CPU/配额及预算选择，最大 32 | 真实事件线程；Linux 查询 cgroup 祖先配额与 affinity，Windows 查询 affinity/可读取 Job CPU 配额，macOS 查询可用 CPU |
| `KU_BLOCKING_THREADS` | 按线程预算选择，最大 64 | HTTP 与 Task 后台线程合计，不随循环数相乘 |
| `KU_THREAD_BUDGET` | 16 / 96 | native main、输出、事件与后台线程总额；不等于整个部署线程数 |
| `KU_THREAD_STACK_BYTES` | 2 MiB / 64 MiB | 事件、后台、输出线程的实际栈预留；须为 4096 倍数且至少 256 KiB；不等于 RSS |
| `KU_TASK_MAX_BYTES` | 16 MiB / 1 GiB | HTTP Task driver、固定记录与已接入拥有数据账本，不是任意 Task 堆的精确总额 |
| `KU_HTTP_REQUEST_TIMEOUT_MS` | 30000 / 300000 | 接受请求开始至排队、读取、执行和写出共用的绝对期限；处理阶段原有期限也继续生效 |
| `KU_HTTP_DRAIN_TIMEOUT_MS` | 5000 / 300000 | 最终排空预算；无法物理收口时以退出码 75 结束服务进程 |

含 Task 文件/网络适配时，从后台总额保留一个共享 Task 后台线程，其余为 HTTP 同步 handler 线程。因此显式四循环的混合服务至少配置五个后台线程；配置不足明确失败，不以事件线程内同步执行兜底。没有显式线程数时可在连接/等待/后台份额范围内缩小默认循环数。固定录入容量由现有 Task driver（1024 槽）、HTTP 活动上限和进程字节账本共同约束。

`http.server` 原有 max_connections/max_active_requests/max_pending_requests/max_buffer_bytes 是**整个实例**的额度。活动/排队槽按循环分份，缓冲字节共享；不存在 N 份完整预算。缓冲计入原始头/体、头字段表、请求字段投影、拥有的响应 backing，固定数组、线程栈和任意用户堆另行说明。响应在产生后的交付点准入，不能把它描述为所有用户内存分配之前均已精确计费。

管理 ingress 使用独立 loopback 端口、一条物理连接、64 KiB 缓冲和 200ms 绝对期限；业务额度耗尽仍可读取实际循环健康与发起 drain。输出固定 256×4096 字节队列和一个计费线程。没有新增长期对象缓存；分配器可能保留已经释放的高水位容量，验收同时检查逻辑责任人、句柄与 RSS 变化。

这些运行时账本与 Linux cgroup v2 / Windows Job 限额是不同层次。硬限制覆盖被控服务进程及第三方库；supervisor 在该域之外，单 Task 全堆未精确计量。macOS 普通服务恢复可用，原生硬内存隔离模式明确拒绝。多个实例及滚动共存的总额由部署配置再相加，不宣称整个系统内存绝对封顶。

## 常驻服务与平台边界

`ku supervise` 仍是有限批次程序执行；常驻实例使用 `ku service`。后者包含 supervisor、小型 launcher 与 native 服务进程；唯一重启负责人是 supervisor，宿主管理器只托管 supervisor。新代次使用新 token，startup/ready/live 依据真实事件循环进度；drain 后旧 keep-alive 不能重新准入业务。原生调用不返回时仍计真实占用，达到既定期限以退出码 75 退役整个服务进程，确认回收后才替换。

```sh
ku service --help
python3 -B deploy/service/manage.py check /absolute/path/cluster.json
python3 -B deploy/service/manage.py status /absolute/path/cluster.json
python3 -B deploy/service/manage.py roll /absolute/path/cluster.json a spare --program /absolute/path/new/service
python3 -B deploy/service/manage.py rollback /absolute/path/cluster.json
```

首次启动、token、两活动实例/一备用位、代理生成和 stop 的完整命令见[参考部署](../deploy/service/README.md)。示例为私有 loopback HTTP，不代表已经部署公网或启用 TLS；生产入口可采用现有代理终止 TLS。HAProxy 配置 `retries 0`、禁止 redispatch，不重放失败 HTTP/SQL。更新预算计入新旧三个实例，旧二进制与配置保留供回滚。

| 平台 | 实际实现 | 本轮验证状态与限制 |
| --- | --- | --- |
| Linux | poll readiness、多循环/Task、普通常驻恢复；可选 cgroup v2 memory/pids 域 | 已接入三平台 CI，最终提交尚待实际运行。硬模式要求可写的独有空委托及祖先允许迁移/启用控制器；相关 CI 在临时 runner 上执行这一项隔离验证，不降低运行时要求 |
| Windows | WSAPoll readiness、多循环/Task、Job containment；可选 Job 提交内存限制、Task Scheduler 宿主 | 已接入实际注册/运行/停止/注销测试，尚待 runner 结果。没有 IOCP/overlapped 后端，不以其完成模型冒充当前实现；提交内存不等于 RSS，分配失败不必然是 OS 杀进程 |
| macOS | poll readiness、多循环/Task、普通常驻恢复、launchd 模板 | 本机已验证下列短矩阵及普通恢复；硬模式实际在应用运行前拒绝。没有提供原生等价硬上限，RSS 定时检查不作为替代 |

Linux systemd、macOS launchd 模板已经提供，本机没有安装宿主 unit/plist；普通隔离实例与 HAProxy 的真实恢复已单独执行。Windows 宿主验收必须由对应 runner 的实际 Task Scheduler 结果证明。被控 OS 域覆盖 launcher、native 进程及第三方库分配，supervisor 自身仍在域外；多个实例部署准入不等于全机绝对硬上限。数据库结果未知如实上报，不承诺所有 OOM 都能变成普通 Result。

## 指标与复现

只有配置内部 token 后才启用 `/__ku/metrics`；通过独立管理端口读取，使用 `X-Ku-Control-Token`。标签为固定 loop id 和有限状态，禁止任意 URL、SQL、请求 ID 进入指标标签。包括每循环准入/结果/队列字节/循环延迟、Task ready/waiting/running/驻留/调度延迟、真实后台占用、数据库额度、缓冲峰值与输出丢弃。

```sh
python3 -B tests/acceptance/runtime_matrix.py --ku /absolute/path/ku --profile release --blocking-regression /absolute/path/native_task_adapter_test --output /absolute/path/runtime.json
python3 -B tests/acceptance/http_pg_lifecycle.py --ku /absolute/path/ku --loops 4 --output /absolute/path/pg.json
python3 -B tests/acceptance/http_pg_lifecycle.py --ku /absolute/path/ku --loops 2 --profile release --performance-seconds 5 --output /absolute/path/pg-perf-2.json
python3 -B tests/acceptance/multiloop_load.py --ku /absolute/path/ku --loops 4 --duration 1800 --rps 50 --output /absolute/path/load.json
python3 -B deploy/service/verify.py --ku /absolute/path/ku --haproxy /absolute/path/haproxy --output /absolute/path/service.json
python3 -B deploy/service/verify.py --blocking-only --ku /absolute/path/ku --output /absolute/path/retirement.json
```

独立 CI 正式入口及门槛见[正式计划](multiloop-formal-plan.md)，只用指定 PR 标签启动，不随普通提交反复长测。

计划与门槛在发流量前落盘。客户端固定线程/在途/队列/直方图，采样线程不占到达率时钟，不重放失败请求；已超过 fixture 的 1 秒空闲期限的客户端连接在发送下一请求前主动回收。过载的慢体连接上限 280，补连也不超过每秒 280 次，其拒绝、期限和传输关闭单独报告；撤除负载后五秒内必须恢复。临时源码、native 产物、数据库和子进程由驱动清理，仅保留精简结果与必要失败证据。

正式窗口参数为：release native、N=4、总后台 5、总线程配置 16、Task 16 MiB、连接 256/活动 128/排队 128/HTTP 缓冲 8 MiB；warm 60 秒、steady 1470 秒、overload 30 秒、recovery 240 秒，共 1800 秒。普通阶段 50 到达/秒，过载 150 到达/秒并叠加 280 条慢体连接；固定 32 个客户端工作线程、64 个在途/队列上界。handler/header/body/write/idle 均 1000ms，`/timer` 等待 25ms。普通阶段必须无非预期错误、无压测端主动丢弃且 p99≤250ms，过载前累计 loop delay 峰值≤100ms；过载必须无错 body，撤载后五秒内正确响应并恢复逻辑队列。错误、拒绝、超时、连接关闭和压测端错误保留各自分母。

选择 30 秒过载是在正式计时前约束同机压测端的端口预算：280 条慢体连接每秒周转，60 秒可能耗尽 macOS 16384 个临时端口；30 秒仍超过服务连接额度，但不以端口耗尽伪造服务故障。没有提高 Ku 期限。过载后的 loop delay 指标是全进程累计峰值，不能据此宣称得到了独立的 recovery 峰值。**本段是冻结计划，正式结果尚未产生，不能写为已通过。**

最新 `final-preflight.json` 短预检的 250 个请求中出现 2 个 `/hot` 504，未达到普通阶段错误 0 的门槛，因此尚未开始正式 30 分钟。该新样本继续做有界诊断，不根据两次超时推断线程卡住、系统竞争或历史七次 504 的原因；早期短矩阵通过不能覆盖它。

本轮实际发现并修复的问题和前后对照见[Task 推进记录](multiloop-task-progress.md)。所有数据仅代表列明的构建、硬件和短/长期窗口，不等价于历史 504 根因已查明或线上长期可用率承诺。

## 工作包状态

| 工作包 | 已落地材料 | 尚待收口 |
| --- | --- | --- |
| W1 现状与设计 | 本文文件/调用链、事件层选型、线程与资源归属表 | 无新增调研事项 |
| W2 多循环运行时 | 固定 owner、真实 HTTP/TCP readiness、timer、有限 ready/IR 预算、可靠合并 wake；对应 source/adapter/HTTP 回归 | 最终提交三平台结果 |
| W3 阻塞与资源 | 有界后台、排队取消、运行者保留真实占用、总额度、管理余量、输出有界；短矩阵和阻塞退役已有实测 | 最终平台硬模式及正式负载资源趋势 |
| W4 HTTP/DB | 同步/async handler 正常入口、PG/MySQL/Redis 有界后台、全实例 DB 账本；真实 PG 成功/故障与短性能已测 | 三平台真实 PG 生命周期 CI 结果；正式 HTTP 窗口 |
| W5 服务恢复 | `ku service`、探针/退避、原生作业超期退役、实际 HAProxy 替换/更新/回滚及永久入口 | 最终提交平台相关恢复 CI |
| W6 平台与部署 | Linux/macOS/Windows 接线、宿主模板、Windows 实际宿主与三平台 CI 入口、多主机配置 | 三平台 CI 实际执行；A19 缺授权双宿主/入口/数据层条件 |
| W7 整体收口 | 聚焦永久回归、证据归档、使用说明与清理约定 | 最终提交/PR、必要 CI、正式 30 分钟、最终产物/进程清理核对 |

## 已取得的对照数据

同机 Apple M1、8 个逻辑 CPU、8 GiB、macOS 26.5.2 arm64；负载机与服务同机，未隔离其他系统工作。短运行时矩阵是 release native，无代理/TLS/数据库，5 秒 CPU5000 与 5 秒 50 到达/秒混合公平性；PG 对照另用私有 PostgreSQL 17.11、固定 4 连接/8 等待者、8 个持久 HTTP 客户端、各 5 秒。二者不是同一种负载。

| 指标 | N=1 | N=2 | N=4 |
| --- | ---: | ---: | ---: |
| 空闲进程线程 / FD | 8 / 9 | 9 / 11 | 11 / 15 |
| 空闲 CPU（单核百分比） | 0.00% | 0.00% | 0.33% |
| CPU5000 请求/秒 | 4177.73 | 3982.90 | 2177.21 |
| CPU5000 p95 / p99 | 2.0 / 2.4ms | 2.4 / 3.5ms | 4.8 / 18.3ms |
| 混合普通请求 / timer p99 | 26.7 / 28.0ms | 2.3 / 33.4ms | 1.3 / 28.1ms |
| 收尾 RSS 比基线增加 / FD 增加 | 2899968 B / 0 | 245760 B / 0 | 245760 B / 0 |
| PG 读请求/秒（全部成功） | 5808.41 | 5709.50 | 5450.24 |
| PG p95 / p99 | 3.0 / 4.2ms | 3.0 / 4.1ms | 3.1 / 4.4ms |

N=2/4 的每个循环均有实际请求量，原生 CPU 总时间也表明确有并行推进；不承诺吞吐线性增加。CPU5000 的 N=4 仍比 N=1/2 慢，共享 driver 管理成本尚存；N=1/2 的同机 Python 发生器亦接近占满单核，因此这些是端到端观察，不是隔离的服务器扩核上限。无证据收益的 frame dirty/copy-quantum 优化已撤回，只保留已证实有效的 owner 定向通知、pump 中自通知合并与 64-dispatch 推进。没有为证明选型追加优化。

PG 总连接固定 4，不随 N 放大；三档请求分别 29048/28559/27274 全部正确，pending/bytes 收尾均零，退出后数据库连接零。旧代理性能轮次有 49 次 `JSONDecodeError`，缺少响应 body，根因未确认；后续有界诊断 10914/10914 和 11537/11537 成功不能倒推旧错误已修复。故障模式仍使用 Python 故障代理，性能模式直连 PG；不同拓扑/连接策略的数据不能合并成 Ku 前后加速比。

服务恢复本机实测：native 退出后替代可服务 286.342ms；滚动 1069.792ms；回滚 631.668ms，回滚内容 20/20 正确。原生 FIFO 阻塞样本在约 1001ms 返回 504，真实工作仍占槽，约 2135ms 摘 ready、2580ms 替代可服务，旧实例退出 75、另一实例后续 10/10 正确，不重放操作。常规部署 8 进程/32 线程/RSS 求和 65.5MB；滚动共存 11 进程/47 线程/79.2MB。RSS 求和含共享页重复，不是 PSS 或 OS 硬上限。

可复核摘要：[`runtime-matrix-macos.json`](evidence/runtime-matrix-macos.json)、[`pg-performance-macos.json`](evidence/pg-performance-macos.json)、[`pg-performance-diagnostic-macos.json`](evidence/pg-performance-diagnostic-macos.json)、[`service-recovery-macos.json`](evidence/service-recovery-macos.json)。各份保留自己的提交基线、工作树标记和 CLI 摘要，不能冒充同一最终构建。

## A01—A19 验收状态

“本机已过”只指已列明的 macOS 工作树构建；整项的平台与正式窗口要求仍由最后一列约束。必要 CI 定义位于 [native-three-os.yml](../.github/workflows/native-three-os.yml)，流程已接线不等于执行通过。原 36 次内存恢复矩阵不无故重跑，本次服务硬模式只做针对性接线验证。

| 编号 | 已有本机证据 / 永久入口 | 尚待确认 |
| --- | --- | --- |
| A01 正常 HTTP | N=1/2/4 内容一致、各 owner 实际处理；HTTP 相关永久回归 | 最终提交三平台 CI |
| A02 网络与 timer | 64 条真实 HTTP 读等待、32 timer；TCP 回归 2×24 并发二进制等待，不占逐连接线程 | 最终提交三平台 TCP/Task CI；不能把 HTTP 慢体样本独自冒充 TCP await |
| A03 慢读写 | 4 条慢体返回 408；6 MiB 慢响应受 8 MiB 总账本控制、写期限后退款，普通请求继续 | 最终提交三平台 CI |
| A04 热点公平 | 早期 CPU5000/hot/timer/普通请求混合三档 250/250 正确；最新短预检 250 个请求中 2 个 hot 504，未通过 | 新短预检缺陷定位/必要修复、正式 30 分钟与三平台 CI |
| A05 终态交错 | 3×24 断连/取消/复用、请求期限 504、owned 子任务/结果回归 | 最终提交三平台 CI/工具检测，不宣称穷举全部交错 |
| A06 wake/满 ready | controlled source/adapter 回归覆盖通知合并、满容量、close 后可靠推进 | 最终提交三平台与相关 TSAN 结果 |
| A07 迟到复用 | reactor 代次回归、真实 TCP 超时后 EOF/重连、stale 通知不作用新对象 | 最终提交三平台 CI |
| A08 阻塞真实占用 | 一运行五排队；超时后排队归零、运行者和 4096B 持有，物理返回后归零；独立容量满回归 | 最终提交三平台 CI |
| A09 PG 正常读写 | 私有 PG 真实参数/中文/null/字段、单次写入副作用、10000 行末字段；N=1/4 及性能三档 | 三平台真实 PG 生命周期 CI |
| A10 PG 故障 | 慢查询最终取消、后端终止、提交确认丢失后实际一行且结果未知；连接丢弃、无重放、后续请求恢复 | 三平台真实 PG 生命周期 CI；旧 49 个无 body 错误仍根因未明 |
| A11 过载恢复 | 短矩阵慢 I/O/额度撤除后恢复，逻辑责任人和缓冲归零 | 冻结 30 分钟的过载分类与五秒恢复结果 |
| A12 输出堵塞 | 256 项队列封顶、丢弃可见，阻塞/关闭出口时 HTTP 推进，收尾有界 | 最终提交平台相关 CI |
| A13 启动/退出 | 管理端口冲突和事件 owner 的 calloc/wake/queue 九项注入正确报错回收；自然 drain 退出 0 | 最终提交三平台 CI |
| A14 服务替换 | 真实进程退出与 FIFO 原生等待退役，另一实例继续，旧工作不重放 | 三平台相关恢复 CI，Windows 实际宿主入口 |
| A15 启动连续失败 | 永久 `service_supervision_test` 检查指数退避、窗口上限与退出 | 最终提交三平台 CI |
| A16 drain/更新/回滚 | 旧 keep-alive 不再准入；真实 HAProxy 先新 UP 后旧退役、更新和回滚内容正确 | Linux/macOS 参考部署 CI 与平台生命周期回归 |
| A17 平台限制 | macOS 普通模式已测，硬模式应用启动前实际拒绝；三平台对应入口已接线 | Linux cgroup v2、Windows Job 新服务接线及三平台必要 CI 实际结果 |
| A18 资源回落 | 短矩阵 FD 增量 0、队列/Task/HTTP 字节零、自然退出；PG/服务验证清理无残留 | 正式窗口趋势、最终提交 CI 与交付前最终清理 |
| A19 多主机 | 双宿主代理配置、容量/单点/RTO/RPO 边界和操作步骤已提供 | 未取得授权双宿主、入口切换和数据库故障域条件，真实跨主机演练未执行，不能称通过 |

最终交付仍需补齐：最新短预检两次 hot 504 的有界诊断与必要修复；最终提交与 PR/CI 链接；三平台逐项实际结果和产物；正式 30 分钟报告的请求分母/错误分类/延迟/恢复/资源趋势；最终清理结果。A19 外部条件不足单列，不免除 A01—A18。
