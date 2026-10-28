# 多线程事件循环实施与验收记录

核对日期：2026-10-21（最终CI于2026-10-22 UTC完成）。交付状态：存在真实外部阻塞。A01—A18所列验收已执行通过，包含Windows实际原生代理更新/回滚；A19缺授权多主机、可切换入口和隔离数据库故障域，未执行。原生正式30分钟和最终三平台服务/数据库检查通过；历史失败仍保留，不能从新通过倒推其原因已全部解释。基线为 v0.0.18-r1（`f0bb31f3420cde8e95ba3569f0fbd1af933c27bf`），分支为 `codex/multiloop-runtime`。本分支不回写 v0.0.18 标签，不上传或部署官网。

全量及正式原生窗口冻结源为 `a31c1135ca82e3e15c426a2ef522522038ea8735`；native运行时最后修改在 `9ac5ed9`。Rust协调器的局部修复在 `7859fc5`，最终定向检查源为 `67d78d9fb43a3db258db56268c7e2eb02ef4764c`。相对a31，生产源码只改 `src/service_supervision.rs`；C后端、IR、Cargo和正式负载输入未变，复用对应原生结果，服务控制另验。最终部署适配源 `e40ab1b5faac143f36bdddc008afad05a7abdccc` 只补Python参考部署/验证；Ku编译输入仍与67d相同，Windows Caddy定向run39709674234实际通过。此前 `d1738ef` 等结果保留历史身份，不能替代删除尾部多余通知后的新检查。Cargo仅新增测试专用example，依赖和lock未变；跳过步骤不计通过。

## 当前调用链与方案

- `src/ir/task{,_lower}.rs` 保存 async 状态；`src/backend/c_task*.rs` 输出 Task 状态机、driver、timer、取消与有界阻塞设施。原 driver 有本地 FIFO 与偷取，网络通过阻塞设施；它与 HTTP 事件循环分开。
- `src/backend/c.rs::emit_http_runtime` 输出 HTTP；原样提取为 `src/backend/c_http_runtime.c`，仍由 include_str 嵌入独立产物，没有运行时源码依赖。原来一个 poll/WSAPoll owner，最多 64 个同步 handler 线程。
- PG 使用 libpq 非阻塞接口但在调用线程同步推进；MySQL/Redis 为同步调用。HTTP handler 的有界后台执行是这些适配的运行模式，不宣称数据库全异步。
- `src/process_execution{,/platform}.rs` 是独立程序的 OS 限制和批次协调器，不是常驻 HTTP 实例的重启负责人。

选择复用已有 poll/WSAPoll readiness 操作和解析器，补固定归属循环、总额度、可靠通知；Windows 没有 overlapped operation，不借用 IOCP 的完成寿命承诺。引入 libuv 需要新增 SDK、打包依赖且仍需实现 Ku 自身状态机，因此优先复用已有平台边界。

服务初始化仅一次；route 表发布后只读。事件线程不执行同步 handler；整个进程的事件线程、后台线程、连接和缓冲限额统一。Task 固定归属、不依赖偷取。常驻进程由一个 `ku service` 重启负责人管理，原 `supervise` 的限制不降级。

仅注册 native Task 路由时，每个 HTTP 事件循环保留一个同步后台线程处理 404、405 等回退；Task handler 仍由事件循环推进。同步、混合、空路由和未知 handler 保持原后台配置。Task I/O 的共享后台份额、总线程预算和请求限制不变；此调整减少空闲线程，不承诺 CPU 或吞吐收益。

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
| `KU_THREAD_BUDGET` | 16 / 96 | Ku显式创建的main、输出、事件与后台线程额度；OS/FFI辅助线程另作平台预留和实测，不等于进程所有OS线程硬上限 |
| `KU_THREAD_STACK_BYTES` | 2 MiB / 64 MiB | 事件、后台、输出线程的实际栈预留；须为 4096 倍数且至少 256 KiB；不等于 RSS |
| `KU_TASK_MAX_BYTES` | 16 MiB / 1 GiB | HTTP Task driver、固定记录与已接入拥有数据账本，不是任意 Task 堆的精确总额 |
| `KU_HTTP_REQUEST_TIMEOUT_MS` | 30000 / 300000 | 接受请求开始至排队、读取、执行和写出共用的绝对期限；处理阶段原有期限也继续生效 |
| `KU_HTTP_DRAIN_TIMEOUT_MS` | 5000 / 300000 | 最终排空预算；无法物理收口时以退出码 75 结束服务进程 |

含 Task 文件/网络适配时，从后台总额保留一个共享 Task 后台线程，其余为 HTTP 同步 handler 线程。因此显式四循环的混合服务至少配置五个后台线程；配置不足明确失败，不以事件线程内同步执行兜底。没有显式线程数时可在连接/等待/后台份额范围内缩小默认循环数。固定录入容量由现有 Task driver（1024 槽）、HTTP 活动上限和进程字节账本共同约束。

`http.server` 原有 max_connections/max_active_requests/max_pending_requests/max_buffer_bytes 是**整个实例**的额度。活动/排队槽按循环分份，缓冲字节共享；不存在 N 份完整预算。缓冲计入原始头/体、头字段表、请求字段投影、拥有的响应 backing，固定数组、线程栈和任意用户堆另行说明。响应在产生后的交付点准入，不能把它描述为所有用户内存分配之前均已精确计费。

管理 ingress 使用独立 loopback 端口、一条物理连接、64 KiB 缓冲和 200ms 绝对期限；业务额度耗尽仍可读取实际循环健康与发起 drain。输出固定 256×4096 字节队列和一个计费线程。没有新增长期对象缓存；分配器可能保留已经释放的高水位容量，验收同时检查逻辑责任人、句柄与 RSS 变化。

Windows完整进程成本曾出现实例24线程却只声明17的漏算。部署现要求显式 `per_instance.platform_thread_reserve>=16`，单实例至少event+blocking+9+reserve，并按最大滚动共存数相加；这不改变native的event+blocking+2执行线程配置。原生三进程4/4实例的预留33、Caddy管理34，总133；带Task Scheduler包装器的参考配置再预留其成本，每实例49、总181。OS辅助线程具体归属未全部确认，预留不是内核线程硬限额；实际验收逐实例与全树分别检查。完整字段及保证见[部署说明](../deploy/service/README.md)。

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
| Linux | poll readiness、多循环/Task、普通常驻恢复；可选 cgroup v2 memory/pids 域 | 冻结源全量/矩阵、真实 PG、HAProxy 更新回滚、长阻塞退役、64MiB 私有域及正式窗口实际通过。硬模式仍要求可写独有空委托和祖先允许迁移/启用控制器 |
| Windows | WSAPoll readiness、多循环/Task、Job containment；可选 Job 提交内存限制、Task Scheduler 宿主 | 默认套件、Job提交限额、长阻塞退役、N1/2/4资源回落通过；最终真实PG读写/取消/断线/确认丢失、Task Scheduler实际注册/替代/停止/注销，以及Windows原生Caddy更新/回滚通过。冷态首次使用新增10句柄保留，后续三轮重复及最终采样未继续增长。没有IOCP/overlapped后端；提交内存不等于RSS，分配失败不必然是OS杀进程 |
| macOS | poll readiness、多循环/Task、普通常驻恢复、launchd 模板 | 默认套件、N1/2/4矩阵、长阻塞退役通过；最终HAProxy恢复、滚动、回滚及真实PG通过。已独立复现并修复控制peer配置失败导致协调器退出；本机热点多余通知已修并通过原短预检及新的Linux正式窗口；旧无完整记录样本仍不追认唯一原因。硬模式在应用运行前拒绝，没有原生等价硬上限 |

Linux systemd、macOS launchd 模板已经提供，本机没有安装宿主 unit/plist；普通隔离实例与 HAProxy 的真实恢复已单独执行。Windows 宿主已由Windows runner的实际 Task Scheduler 注册、配置读回、替代和停止结果证明。被控 OS 域覆盖 launcher、native 进程及第三方库分配，supervisor 自身仍在域外；多个实例部署准入不等于全机绝对硬上限。数据库结果未知如实上报，不承诺所有 OOM 都能变成普通 Result。

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

选择 30 秒过载是在正式计时前约束同机压测端的端口预算：280 条慢体连接每秒周转，60 秒可能耗尽 macOS 16384 个临时端口；30 秒仍超过服务连接额度，但不以端口耗尽伪造服务故障。没有提高 Ku 期限。过载后的 loop delay 指标是全进程累计峰值，不能据此宣称得到了独立的 recovery 峰值。正式执行结果见下节，仍按原冻结门槛判定。

macOS旧预检2/250个hot504保留，原两个样本缺少内部映射，不能逐一补写唯一原因。后来有界捕获到新的完整504，确认driver锁内多余自通知放大等待：通知区间69.973/80.264ms、另一owner取锁120.623ms。只删除该冗余通知后，原5秒/250到达无诊断release预检250/250正确，p99=28.5ms，loop delay峰3ms，退出0；其他off-CPU区间并未全部唯一归因。[关联证据](evidence/mac-tail-notify-diagnostic.json)区分了墙钟/CPU与仍未知部分，不据此解释历史七次504。新冻结源重新接受正式窗口，不能沿用旧d173通过。

本轮实际发现并修复的问题和前后对照见[Task 推进记录](multiloop-task-progress.md)。所有数据仅代表列明的构建、硬件和短/长期窗口，不等价于历史 504 根因已查明或线上长期可用率承诺。

## 最终源 CI 与正式窗口

- [最终服务复验39705058328](https://github.com/fengyanweb/ku/actions/runs/39705058328)：源 `67d78d9`，实际三平台运行。仅Rust协调器控制连接出错路径改变，已接受peer的模式/期限配置失败只丢该peer，明确清除继承的nonblocking模式；新跨平台永久回归检查同协调器/同代次仍存活、业务正确、正常stop0。三平台fmt/clippy、服务回归和真实PG均通过；服务回归Linux/macOS各11/11，Windows10/10（Unix专属一项不编入）。Linux/macOS的HAProxy退出替代、滚动、回滚及Windows实际Task Scheduler均通过，清理错误全空。此前39703806250的Linux/Mac缺rustfmt已补组件；Windows当轮PG超时后首个read触发JSONDecodeError，缺正文无法唯一归因，新报告增加2KiB响应/phase而保持断言、次数、期限不变。本轮通过不证明那一次已修复。
- [Windows代理最终复验39709674234](https://github.com/fengyanweb/ku/actions/runs/39709674234)：部署源 `e40ab1b`，实际原生Caddy2.11.7的退出恢复、更新、回滚和逐实例/全树资源检查通过。未改Ku执行线程或期限。前轮547在共存89线程超过原声明85时真实失败；已按完整进程成本修正Windows显式平台预留、启动前拒绝不足预算，并增加逐slot校验，原失败不改为通过。
- [全量 CI 39698998227](https://github.com/fengyanweb/ku/actions/runs/39698998227)：冻结源 `a31c113`，PR merge `cd49b394267fd2dcdb87b5c7d565314e64340b7e` 与 head 的 tree 同为 `626d3a7bd0fedcc981f5d0d7ffd0a32c813999ff`。实际10个job成功、2个失败、3个发布条件跳过，整体失败。三平台默认回归（Linux/macOS各2007项、Windows2001项）、quality、native/CheckOnly及Linux ASan/UBSan/泄漏和TSAN通过。Linux资源、PG和服务全部通过；Mac服务恢复与Windows PG验证端两项原失败保留。
- [Mac定向 39701941690](https://github.com/fengyanweb/ku/actions/runs/39701941690)：`5b78aa8`新增有界诊断；native退出替代曾在655.843ms就绪、另一实例10/10，但随后滚动前协调器退出1，故该轮失败。阶段/PID/最多12帧/进程回执保留，缺少旧轮stderr，不能补写其唯一原因。
- [Windows定向 39702448682](https://github.com/fengyanweb/ku/actions/runs/39702448682)：同一 `5b78aa8` 的真实PG与TaskScheduler通过。psql命令行字面量实际变成 `3f3f3f3f207061796c6f6164`（四个问号），而实库唯一一行的payload是正确18字节UTF8；全ASCII观测保留完整ID/payload字节断言。确认这是该验证端比较失真，未修改Ku/libpq写入，也没有重放SQL。提交确认丢失仍实库恰好1行、reported unknown；drain0、退出后PG连接0。真实LocalService宿主替代2781ms、另一实例10/10、停止0且无进程残留。
- [正式30分钟 39699000190](https://github.com/fengyanweb/ku/actions/runs/39699000190)：干净 `a31c113` 的native路径完整通过。普通88,500次全部正确，普通各阶段p99≤25.6ms、循环延迟峰值1ms。过载4500到达=1460正确+215 HTTP503+19对端传输终止+2806发生器额度拒绝；慢体连接另计。撤载189.279ms恢复，最终队列/Task/HTTP字节零，native退出0。环境、错误分母与成本见[正式报告](multiloop-formal-plan.md)和[原始摘要](evidence/formal-30m-linux.json)。

正式窗口native采样线程11、RSS基线2,396,160B/峰值3,244,032B、FD基线及各阶段结束15/峰值270。RSS高水位保留，不冒充返回初始RSS或无限期无泄漏证明。`timeout_responses`中的慢体/空闲408与完整测量请求错误率不同分母。后续控制连接修复只涉及Rust协调器；C发射/native运行时/正式驱动和fixture未改，本窗口不代替协调器的另项验收。

此前d173全量/正式窗口及Windows存在性、资源测量、私有PG环境和输出回收的失败/修正过程集中保留在[推进记录](multiloop-task-progress.md)和[CI证据](evidence/multiloop-platform-ci.json)。旧通过不覆盖新失败，定向跳过的默认、内存和资源步骤也不新增通过。没有以PR事件跳过的发布任务冒充新版本发布。

## 工作包状态

| 工作包 | 已落地及实测 | 剩余边界 |
| --- | --- | --- |
| W1 现状与设计 | 文件/符号映射、实际后端选型、线程/资源归属 | 完成 |
| W2 多循环运行时 | 固定owner、HTTP/TCP readiness、timer、有限批次/IR预算、可靠wake；三平台回归、热点修复及新正式窗口通过 | 不是任意同步代码的强制抢占 |
| W3 阻塞与资源 | 有界后台、queued取消、物理占用、总额度/管理余量/输出；三平台限制、退役和资源回落通过 | 不等于所有Task堆精确计量 |
| W4 HTTP/DB | 同步/async入口、DB有界后台/全实例账本；三平台真实PG成功与故障通过 | DB保持既有同步有界模式，不重放SQL；旧缺正文样本原因未确认 |
| W5 服务恢复 | ku service、探针/退避/退役；实际HAProxy退出替代/更新/回滚和Windows宿主通过；控制RST回归通过 | systemd/launchd提供模板，不冒称已安装在用户生产宿主 |
| W6 平台与部署 | 三平台接线、实际CI、宿主模板、双宿主配置及完整演练步骤 | A19缺授权双宿主、入口切换及隔离数据层 |
| W7 整体收口 | PR、必要永久回归、正式Linux30分钟、精简原始证据、文档与自有临时清理 | A19真实外部演练未执行；没有新版本发布或官网部署 |

## 最终服务与数据库结果

源 `67d78d9`、run39705058328三平台均成功。下表是本轮实际墙钟，不是跨平台性能排名：

| 平台 | 服务回归 | 实例退出至替代可服务 | 滚动 / 回滚 | 真实PG |
| --- | ---: | ---: | ---: | --- |
| Linux | 11/11 | 241.664ms | 523.164 / 486.591ms | 压力5 acquire_timeout、7 execution_unknown；确认取消8 |
| macOS | 11/11 | 730.645ms | 1274.199 / 1754.614ms | 压力4 acquire_timeout、8 execution_unknown；确认取消9 |
| Windows | 10/10 | Task Scheduler 2844ms | Unix HAProxy不在Windows安装 | 压力8 acquire_timeout、4 execution_unknown；确认取消5 |

Windows原生Caddy最终 `e40ab1b` 的run39709674234通过：退出替代219ms，另一实例10/10；滚动17.031s，回滚15.969s且旧内容20/20。15s配置是各ready/health/retire阶段的既定期限，不是整次roll的总墙钟限额；其中等待新配置后的实际健康指标刷新至少10.1s，旧/其余实例仍可服务。没有提高任何Ku请求期限或执行线程数。

| Windows Caddy进程成本 | 稳态 | 三实例共存 | 回滚后 |
| --- | ---: | ---: | ---: |
| 全树进程 / 线程 | 8 / 64 | 11 / 88 | 8 / 56 |
| 全树RSS求和 | 103,043,072B | 121,094,144B | 105,902,080B |
| 管理Python+Caddy线程 | 16 | 16 | 16 |
| 管理RSS求和 | 69,242,880B | 70,877,184B | 72,687,616B |
| 每实例实际线程 | a24、b24 | a24、b24、spare24 | a24、b16 |

普通三进程实例单份预留33、管理34、滚动总133，各阶段逐slot/管理/总额检查均无错误；管理内存额外128MiB，服务三份各512MiB，全部实测低于声明。它们是准入预留与采样结果，不是OS线程硬上限。四个自有协调器退出0，Windows显式终止代理回执1，最终采样PID清空、cleanup_errors=[]。原547共存89>85的真实失败和修正依据保留，不算新通过或Caddy线程失控。

三平台均逐字段核对参数/中文/null、单次UTF8写入18字节、10000行末字段；提交确认丢失时实库恰好1行、reported execution_unknown、replayed=false。超时、连接中断后新请求恢复，最终pending/bytes=0、正常drain=0、退出后数据库连接=0，cleanup_errors=[]。压力错误来自有意设置的短期限，完整分类保留，不能算作正常业务成功。

Windows真实LocalService（S-1-5-19）三个独有任务的动作/三次一分钟间隔恢复配置均读回；受控长期等待在1032ms返回504，旧实例退出75、代次1→2、另一实例10/10正确；停止退出0、任务全部注销、临时树进程为空。Linux/macOS的HAProxy回滚各核对20次正确内容，全部自有协调器退出0。

曾独立复现的控制连接问题只涉及Rust协调器：RST后accepted peer设置读写期限在Darwin返回EINVAL，旧实现向上传播后退出整个协调器；现显式清除继承nonblocking、配置失败只关闭该peer。永久回归在旧代码失败，修后本机及三平台通过，保持同协调器/同代次及正常stop。旧Mac CI没有stderr的退出样本不能据此补写唯一原因。上一轮Windows PG缺正文的单次JSONDecodeError也不解释成JSON损坏、迟到取消或已修复；已保留失败、增加有界记录，本轮原断言通过，没有提高期限或重放SQL。

## 本次原生执行路径的同机 1／2／4 对照

最终 `a31c113` 的 Linux 全量 CI 在同一 runner、同一构建和负载下执行三档。逻辑 CPU、affinity 和 cpuset 均为 4，所记录可见域 `cpu.max` 为 `max 100000`；该短矩阵环境记录不冒充正式窗口的全祖先 quota 核查。请求处理量证明每个循环实际参与，短负载的 native CPU 占用低，不能据此承诺 CPU 饱和容量或线性收益。

| 指标 | N=1 | N=2 | N=4 |
| --- | ---: | ---: | ---: |
| 空闲线程 / FD | 8 / 9 | 9 / 11 | 11 / 15 |
| 空闲 CPU（单核百分比） | 0% | 0% | 0% |
| 空闲 RSS | 2,437,120B | 2,453,504B | 2,486,272B |
| CPU5000 请求/秒 | 196.036 | 195.878 | 195.851 |
| CPU5000 p95 / p99 | 41.3 / 42.1ms | 41.9 / 42.5ms | 41.9 / 42.5ms |
| 每循环 CPU 路由累计请求 | 984 | 492 / 492 | 246 / 246 / 246 / 246 |
| native CPU 增量（约5.02秒窗） | 0.29s | 0.29s | 0.29s |
| 发生器 CPU | 0.286s | 0.282s | 0.267s |
| 收尾 RSS 比基线增加 / FD 增加 | 315,392B / 0 | 507,904B / 0 | 585,728B / 0 |

本组端到端吞吐未随循环数增加，未观察到 native CPU 饱和；不能据约41ms分位数补写网络、系统或锁的唯一原因。混合公平性、真实等待、资源回落、PG成功/故障及进程清理由同一最终CI的独立项目验收。完整平台摘要见[最终CI证据](evidence/multiloop-platform-ci.json)。

## 修复过程的对照数据（各历史构建）

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

三平台默认套件覆盖具体生命周期/失败注入；验收脚本的实际平台结果补充黑盒与依赖链路。已有36次内存恢复矩阵未重跑，新服务限制只做对应接线验证。A01—A18按列明范围通过；已解释的具体缺陷、旧未解释样本和A19外部条件分别保留。

| 编号 | 实际覆盖 | 当前边界 |
| --- | --- | --- |
| A01 正常 HTTP | 三平台N1/2/4内容一致、每循环有实际处理量 | 资源成本另列A18，不承诺线性扩核 |
| A02 网络/timer | 两批24条Task TCP binary等待、EOF/timeout/reconnect；HTTP64等待、timer32等待，固定线程 | 真实网络等待与不完整HTTP体分开计数 |
| A03 慢 I/O | 三平台慢体/6MiB慢响应，字节额度有效，其他请求推进 | OS socket缓冲不冒充逐Task堆 |
| A04 热点公平 | 三平台短矩阵、修后本机250/250及新Linux30分钟普通88,500次零错误 | 锁内冗余通知已修；旧两个无完整关联样本不补写唯一根因 |
| A05 终结交错 | 三平台原期限、取消/断连、owned释放和自然drain回归通过 | sanitizer/注入不等于穷尽全部交错 |
| A06 通知 | 固定owner、重复wake、容量满和关闭永久回归，TSAN相关路径通过 | 没有任意线程抢占承诺 |
| A07 复用 | 三平台旧generation/wrong-owner/TCP close与重连通过；提取请求别名越界读已复现修复 | Linux检测含泄漏，本机完整53个检测目标通过 |
| A08 阻塞 | 一运行五排队；超时后queued归零、运行者及4096B保留至实际返回；满池回归通过 | 永不返回者按进程退役边界回收 |
| A09 PG 正常 | 三平台真实字段/中文/null/参数、单次UTF8写入、10000行末字段正确 | Windows验证端Unicode argv失真已用实库字节确认并修正 |
| A10 PG 故障 | 三平台慢查询/后端终止/提交确认丢失：实际恰好1行、execution_unknown、不重放、后续恢复 | 旧49个性能错误及7859一次超时后read缺body，原因未唯一确认；保留有界诊断 |
| A11 过载 | Linux正式分类保留全部分母，撤载189.279ms正确响应且队列/Task/字节归零 | 一个固定窗口，不宣称各平台相同容量 |
| A12 输出 | 三平台固定256项队列、丢弃/写失败可见、HTTP推进、有界退出 | 默认supervisor空设备输出不等于持久日志 |
| A13 启停 | 三平台九项owner初始化失败注入、端口冲突、自然drain退出0 | 没有半启动假ready |
| A14 实例替换 | 三平台真实native长阻塞退役、另一实例10/10、回收后新代接流；Windows实际宿主通过 | 数据库结果未知不推断回滚 |
| A15 连续失败 | 三平台真实service_supervision回归检查退避、频率上限与停止 | 宿主只管理supervisor，不重启业务第二次 |
| A16 更新回滚 | Linux/macOS实际HAProxy及Windows实际原生Caddy先确认新健康再摘旧，更新/回滚内容正确；三平台drain/keep-alive回归 | Windows使用Caddy原子load及固定健康路由，未安装Unix HAProxy；独立源与实际资源见本页CI和结果 |
| A17 平台限制 | Linux64MiB域真实事件/空域回收；Windows64MiB Job提交限额；macOS启动前拒绝硬模式 | supervisor仍在硬域外，单Task全堆未精确计量 |
| A18 回落清理 | 三平台短矩阵、Linux正式资源/退出、PG/服务实例清理通过；Windows首次后同进程三轮及最终不再增句柄 | 冷态首次+10句柄仍保留且归属未唯一确定；RSS保留高水位不冒充零内存 |
| A19 多主机 | 双宿主配置、操作入口、容量/单点/RTO/RPO边界已提供 | 未取得授权双宿主、入口切换与DB故障域，真实演练未执行 |

以上是原 A01—A19 的范围记录：A01—A18 列明检查通过，A19 提供配置和可执行步骤，但没有授权多宿主、可切换入口及隔离 PG 复制/故障域，真实跨主机演练未执行。新增 B01—B08 的执行关系、修复、正式容量和最终 CI 单独见 [多核与并发容量验收](multicore-capacity-acceptance.md)；A19 不能替代这些单机责任。不能用单机进程故障冒充主机故障，也不能将旧证据不足样本写成修复。官网、v0.0.18 发布、旧 JSON 和两项暂停分析保持不变。
