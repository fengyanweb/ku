# 受控进程执行与内存限制

`ku supervise` 把每个提交的完整程序放入新的 worker，由协调进程管理准入、期限、输出和退出清理。它已经接入 CLI；实际平台验收与限制见文末。

执行单元是整个 Ku 程序。它不是“每个 HTTP 请求一个进程”，也不把一个程序中的每个 Ku Task 拆成进程。普通 `ku run`、直接启动独立产物及嵌入式解释器调用不会自动获得这里的限制；需要通过 `ku supervise` 启动。

## 使用方式

解释器程序或包入口：

```sh
ku supervise \
  --worker-memory-bytes 134217728 \
  --total-memory-bytes 536870912 \
  --max-workers 2 \
  --max-queued-jobs 32 \
  --max-queued-bytes 262144 \
  --max-output-bytes 1048576 \
  --deadline-ms 30000 \
  --cgroup-root "$KU_CGROUP_ROOT" \
  -- ./examples/hello.ku ./examples/closure.ku
```

这个示例用于 Linux，`KU_CGROUP_ROOT` 必须指向已委托的 cgroup v2 目录，条件见后文。Windows 使用同样的参数，但省略 `--cgroup-root`。参数值使用十进制字节数或毫秒数，不接受 `128MiB` 等单位后缀。`--` 必须存在，后面的每个路径都是一个独立执行单元。

协调进程只规范化路径、检查文件类型并保存有界描述符；不读取、检查或展开 Ku 源码。解释器 worker 实际执行当前 Ku 可执行文件的 `run --offline <规范化路径>`，因此源码读取、包展开、检查及运行都发生在受控 worker 中。需要的依赖应先准备好；这个入口不会在依赖缺失时自动联网安装。

native 程序需要先构建，再交给同一个管理器：

```sh
ku build --backend c ./examples/hello.ku -o ./worker
ku supervise --native \
  --worker-memory-bytes 134217728 \
  --total-memory-bytes 536870912 \
  --cgroup-root "$KU_CGROUP_ROOT" \
  -- ./worker
```

Windows 的产物通常为 `worker.exe`，仍须省略 `--cgroup-root`。这一模式运行预编译文件，不在协调器中编译。构建命令自己的编译器资源不包含在随后的 worker 限制内。当前入口不传递额外应用参数，worker 的标准输入关闭。

一个批次可以运行多个相互独立的程序。某个程序失败后继续处理其他程序；全部完成后，只要有一个失败，协调器最终返回非零。失败程序不会自动重试，也不会自动重新启动长期服务。

## 三种不同的预算

| 层次 | 计量对象 | 能提供的保证 |
| --- | --- | --- |
| 运行时分配账本 | 已接入计费点的 Task 帧、header、payload、HTTP 缓冲等容量 | 可以在相应分配前拒绝；没有覆盖的分配不在这个账本中 |
| worker 进程资源域 | 受控程序及仍属于其资源域的后代，按平台内核定义计费 | 包含 Ku 与 MySQL、libpq、TLS 等库通过该平台常规内存机制产生的计费量；不依赖库提供自定义 allocator |
| 单 Task 限额 | Task 自身的既有帧、载荷、等待和运行规则 | 不等于一个 Task 的完整堆内存；不能精确拆分共享连接池、缓存和库内部内存 |

同一 worker 内的 Task、HTTP 请求、数据库连接池和缓存共享 worker 限额。连接池归创建它的 worker，协调器不持有或跨 worker 共享数据库连接。worker 被终止时，不能依赖 Ku 的 `finally` 或第三方库析构继续完成。

外部数据库服务、远端服务、构建编译器、协调器自己的 RSS、宿主系统及其他程序不属于 worker。进程资源域也不是所有物理内存、映射、设备分配和共享页的统一精确计量器。

## 默认值与边界

| 参数 | 默认值 | 最大值或附加条件 |
| --- | --- | --- |
| `--worker-memory-bytes` | 134217728，即 128 MiB | 1099511627776，即 1 TiB；Linux 还要求按实际内核页大小对齐 |
| `--total-memory-bytes` | 536870912，即 512 MiB | 17592186044416，即 16 TiB；必须容纳控制预留和至少一个完整 worker |
| `--max-workers` | 2 | 64 |
| `--max-queued-jobs` | 32 | 4096；限制本批次提交的全部入口数 |
| `--max-queued-bytes` | 262144，即 256 KiB | 16777216，即 16 MiB |
| `--max-output-bytes` | 1048576，即 1 MiB | 16777216，即 16 MiB；每个 worker 的 stdout、stderr 合计 |
| `--deadline-ms` | 30000 | 86400000，即 24 小时 |
| `--cgroup-root` | 无 | Linux 必填；Windows 不接受；路径最长 32 KiB |
| `--native` | 关闭 | 开启后入口必须是预编译普通文件 |

数值必须为正；重复或未知参数在启动用户程序前报错。每个原始和规范化入口路径都不能超过 32 KiB。路径队列按各入口原始/规范化表示中较大的操作系统字节长度计费：Unix 为路径字节，Windows 为 UTF-16 存储字节。不排队源码正文或任意大小 IPC 消息。

### 总额与控制预留

记总参数为 `T`，单 worker 参数为 `N`，控制预留为 `C`。代码先计算路径与描述符、输出容量、读取线程、管道余量和固定工作空间，再将剩余 worker 总额向下按 64 KiB 对齐。舍去的字节计入 `control_alignment_padding_bytes`，不会增加可用预算。

```text
worker_group_total_bytes = floor((T - requested_control_reserve) / 65536) * 65536
C = T - worker_group_total_bytes
max_running = min(max_workers, floor(worker_group_total_bytes / N))
```

`max_running` 小于配置的 `max_workers` 是可能的。每次启动前预留完整 `N`，直到进程组退出、直接子进程回收和两个输出 reader 退场后才归还。不会把当时测到的低内存当成可以超卖同一额度的理由。

控制预留按配置上限保守计算，主要组成如下；具体结构大小以构建平台为准，启动记录输出每一项的实际值。

| 项目 | 预留方式 |
| --- | --- |
| 路径 | `4 × max_queued_bytes + 4 × 32768`，容纳 argv、规范化路径和临时表示 |
| 描述符 | 依据排队/活动结构大小、路径副本和固定元数据余量计算 |
| 输出 | 每个 reader 的 Vec 请求容量为 `O`，两个 Vec 合计预留 `2 × O`；两个流实际保留的总长度不超过 `O` |
| 读取线程 | 每个 worker 两个 8192 字节块、两个请求栈大小为 256 KiB 的线程 |
| 管道 | 每个 worker 两条管道，各预留 1 MiB 余量 |
| 固定空间 | 128 KiB，加总额对齐余量 |

这是准入与缓冲容量账本，不是协调器进程的 OS 内存硬限。分配器元数据、线程实际栈/guard、系统对象和内核管道记账不能由这些数值精确还原。Windows 管道对象只限制数量并使用系统管道缓冲，不能把预留 1 MiB 宣称为系统对象的精确物理上限。记录中的 `coordinator_rss_hard_limit` 与 `kernel_object_bytes_exact` 都为 `false`。

## Linux：cgroup v2

Linux provider 使用 cgroup v2 `memory` 与 `pids` 控制器，不使用采样 RSS 或 `RLIMIT_AS` 代替。资源域结构为：

```text
已委托的根目录
└── ku-execution-...             所有 worker 的合计资源域
    ├── worker-1                独立程序 1
    └── worker-2                独立程序 2
```

代码在新建 parent 设置合计 `memory.max`，每个 leaf 设置 `N`；两层都设置 `memory.swap.max=0` 和 `memory.oom.group=1`。每个 leaf 的 `pids.max=256`，parent 为 `256 × max_running`。这里的 pids 数包含内核线程，不等于仅有 256 个用户程序。启动前检查设置回读值，并要求可写的 `cgroup.kill`。

子进程在 `exec` 前通过预先打开的 `cgroup.procs` 加入 leaf，然后才加载执行应用和第三方库；不会先放行用户程序再补资源域。这个 hook 只执行固定系统调用。输出管道也在 `exec` 前请求缩小到 4096 字节，并验证内核接受的容量不超过每管道预留值。父进程死亡信号用于处理启动中的协调器退出竞态，但不替代服务管理器对崩溃残留目录的管理。

指标名为 `linux_cgroup_v2_charged_memory_bytes`。它是内核归入该 cgroup 的计费量，不是 RSS：包含适用的匿名页、文件缓存及内核计费项；共享页不会按每个使用者重复精确分摊，迁移也不会重写此前的所有权。`memory.max` 在特定情况下允许瞬时越限；达到限额后的内核回收、拒绝或 OOM 处理不等于“每一时刻物理内存绝不多一个字节”。HugeTLB、设备/GPU 等另有控制与记账条件，当前 provider 未为它们配置独立限制。[内核计费与限额合同](https://docs.kernel.org/admin-guide/cgroup-v2.html#memory)

`memory_peak_bytes` 来自 `memory.peak`，内核未提供该文件时为 `null`。`memory_current_bytes` 来自 `memory.current`。新 leaf 的事件计数用于区分回收触及限额和实际 OOM：`max` 事件本身不表示程序失败；单独 `oom_kill` 也可能源于宿主全局 OOM。当前实现只在异常退出且同时取得 leaf 本地 OOM 与 OOM-kill 证据时，将结果标为 `memory_limit`；其余情况保留实际退出结果及计数，不猜测原因。

### 委托根条件

由管理员或服务管理器预先提供一个专用、可写、适合创建 domain 子组的 cgroup v2 根目录：

1. 路径必须为绝对路径，解析后确实位于 cgroup v2 文件系统。
2. 根的 `cgroup.subtree_control` 已启用 `memory` 和 `pids`，上级允许这些控制器继续委托。
3. 调用用户能创建、配置、清空和删除子组，能写子组的 `cgroup.procs` 与 `cgroup.kill`。
4. 根应作为委托子组的父节点使用；不要把协调器放入 Ku 新建的 worker 资源域。
5. 内核提供本实现要求的 `memory.swap.max`、`memory.oom.group`、`pids.max` 和 `cgroup.kill` 接口。缺少接口或权限就拒绝启动，不降级为定时检查内存。

可以先只读核对已分配路径，再运行命令：

```sh
KU_CGROUP_ROOT=/sys/fs/cgroup/example-delegated-domain
cat "$KU_CGROUP_ROOT/cgroup.controllers"
cat "$KU_CGROUP_ROOT/cgroup.subtree_control"
ku supervise --cgroup-root "$KU_CGROUP_ROOT" -- ./examples/hello.ku ./examples/closure.ku
```

示例路径只是占位，必须换成实际委托路径。Ku 不为此修改无关父节点，不提供对整个 `/sys/fs/cgroup` 的全局写入命令。宿主已存在的祖先限额仍可比本命令更紧，无法保证 worker 一定能用满 `N`。这也不是对任意外部程序主动改变资源组、使用特权或设备接口行为的通用隔离承诺。

## Windows：Job Object

Windows provider 使用 parent Job 的合计提交量限制，以及每个 worker Job 的合计/单进程提交量限制。每个 worker Job 还限制最多 64 个进程，parent 为 `64 × max_running`；该数不限制进程内线程数量。Job 配置了关闭句柄时终止成员。

进程以 `CREATE_SUSPENDED` 创建，核对初始提交量后分配 parent 与 worker Job、验证配置，再恢复初始线程。 创建过程中由 OS 产生的初始提交并非由事后分配的 Job 追溯拦截；初始量超过 `N` 时在放行应用线程前终止，不能把这一核查描述为覆盖创建阶段每个瞬间的限制。Job 嵌套或分配被当前宿主策略拒绝时，终止并回收暂停的子进程，不放行后再假定限制有效。

指标名为 `windows_job_committed_memory_bytes`，限制的是 Job 定义的可提交虚拟内存，不能与 RSS、Linux cgroup 计费量直接比较。提交被拒绝可能使库返回空指针、处理错误、退出或异常终止；不能保证每次拒绝都会杀进程，也不能保证都转换成普通 Ku Result。[Microsoft 的提交量限制定义](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-jobobject_extended_limit_information)

`memory_peak_bytes` 是内核提供的 Job 峰值。当前实现没有把采样 RSS 冒充当前 commit，也未新增可能累积的通知队列。因此 `memory_current_bytes`、`oom_kill_count`、`memory_limit_event` 和 `memory_limit_termination` 为 `null`，表示未取得该证据；不能据退出码或峰值断言 OOM。

## macOS 与其他宿主

macOS 原生 provider 直接拒绝该受控入口，在拒绝前不启动用户程序。`RLIMIT_AS` 或定期读取 RSS 不被当成与 Linux/Windows 相同的执行机制。传入一个本地普通目录作为 `--cgroup-root` 也不能启用 Linux 控制器。

可行运行方式是在具有所需委托根的 Linux 主机或 Linux VM 客体内安装并显式运行 Linux 版 Ku。当前没有从 macOS 自动转发到 VM 的一键桥接、自动安装 VM 或远程执行协议；客体的 cgroup 限制也不是宿主虚拟机进程 RSS 的限制。

## 期限、输出与失败语义

每个批次的入口在排队时取得单调时钟绝对 deadline；等待、路径检查后的队列停留、启动和执行共用期限，不为每次重试或排队重新续期。到期但尚未启动的入口返回 `queue_timeout`；运行中的入口请求终止。协调器按有限轮询间隔观察期限，之后还需要独立的清理宽限；deadline 不是对 OS 调度延迟或所有阻塞系统调用完成时间的瞬时保证。

stdout、stderr 由两个 reader 并发读取，合计保留上限为 `O`。超出时终止该 worker 并报告 `output_limit`。进程组确认退出后，reader 排空已经缓冲的数据，再在无数据时退出，不等待一个仍被外部持有的写句柄无限关闭。reader 没有完成排空或不能确认退场时，不报告成功。

标准输出为 JSONL：先写 `configuration`，逐项写 `execution`，最后写 `summary`。每项含入口、结果、耗时、排队耗时、退出码、平台指标及剩余预留。`stdout_base64`、`stderr_base64` 使用 Base64 保存原始字节，不要求程序输出为 UTF-8；`output_seen_bytes` 只表示已经观测的字节数，`output_truncated` 表示未完整保留。结果不累计成一个无界列表。

输出的接收方仍需读取协调器的 stdout；外部接收方停止消费会造成背压。这种输出等待不会改变内核的 worker 内存限制，但不应把这种情况下的整体命令返回时间当成已完成 deadline 验收。

结果包括 `completed`、`worker_exit`、`memory_limit`、`timeout`、`output_limit`、`reader_error`、`spawn_error`、`queue_timeout` 和 `cleanup_unconfirmed`。平台不能确认内存原因时保留 `worker_exit`。失败项标记 `side_effects=unknown`、`retries=0`：数据库写入可能已提交，连接断开或 worker 退出都不证明事务未执行，不能盲目重放。

正常与失败路径都终止仍存活的受控后代、回收直接子进程，并等待 reader。只有全部确认后才归还 worker 额度。清理失败时停止接纳后续入口，返回非零并保留未归还额度/资源域证据，不报告预算已经归零。协调器被强制终止、宿主崩溃和无法回收的内核状态不属于 Rust Drop 能保证完成的路径，残留资源域由部署方的上级管理器核对。

## 验收记录

2026-10-18 本机为 macOS 26.5.2 / M1 / 8 GiB，Rust 1.89.0。Linux 实测使用独立 Lima 2.2.1 VZ 客体：Alpine 3.23.4、Linux 6.18.22 aarch64、1 CPU、512 MiB、无 swap。该客体没有共享宿主 home。内存对照使用单 worker 64/192 MiB、总参数 192/300 MiB；委托根另有 320 MiB 上级限制。数据库服务在 worker 域外顺序运行，全部为临时 loopback 数据库和 SELECT，没有业务数据或 migration。

| 项目 | 实际结果 |
| --- | --- |
| 预算、页对齐、合计输出、排空 | 6 个模块回归通过；格式、workspace/all-targets 类型检查、严格 Clippy 与 release lib/bin 构建通过 |
| macOS CLI | 2 项通过；拒绝模式在 marker 程序启动前报错 |
| Linux 真实 provider | 3 项通过，显式委托根已启用；包含解释器成功、native 输出字节、退出/输出限额后继续、3 次实际分配超限、准入/队列/期限/清理 |
| 后代回收 | 同一永久回归实际 fork + setsid，让父程序先失败；后代仍由 cgroup 回收，后续程序成功，资源组为空 |
| OpenSSL 3.5.9 | 96 MiB OPENSSL_malloc 逐页触碰在 192 MiB worker 成功；64 MiB 下 10 次内核终止、10 个后续小程序均成功 |
| libpq 18.6 / PostgreSQL 18.6 | 96 行各 1 MiB 的完整 PQexec 结果在 192 MiB 成功并核对长度/内容；64 MiB 下 10 次终止、10 个小查询均成功 |
| Connector/C 3.4.6 / MariaDB 11.4.12 | 同规模完整 mysql_store_result 在 192 MiB 成功；64 MiB 下 10 次终止、10 个小查询均成功；不把本项当作 Oracle MySQL SDK 的实库矩阵 |
| 真正 Ku native 数据库路径 | 两套标准库生成的 C 均实际链接对应 SDK。高额度的大结果按既有结果预算返回不可交付/执行状态未知错误，无自动重试；64 MiB 时各 3 次内核终止，随后各 3 个小查询均成功 |
| 反复失败与清理 | 上述 36 次外库/Ku native 超限后，各批次 active_workers=0、reserved_worker_bytes=0、所有叶进程数=0、无 Ku domain 留存；临时 DB 进程/目录已回收 |
| 并发和整体准入 | 永久回归将配置 4 并发按总预留收敛为 2，核对峰值 2 个 worker / 128 MiB 预留、排队个数/字节拒绝以及从排队起算的截止 |
| Windows | 对应提交的 Windows CI workspace 将执行真实 Job provider；本机没有把静态代码或 Linux 结果当作 Windows 通过证据，最终结论需核对该任务 |

受控 64 MiB 大请求实测计费峰值最高为 **70,709,248 字节，约 67.43 MiB**，同时记录本地 OOM/终止事件。这说明 kernel memory.max 的瞬时越限边界实际发生了；不能承诺任意时刻 RSS/计费量精确不超过 64 MiB。高额度的三个 SDK 大分配峰值分别为 101,703,680 / 104,366,080 / 104,939,520 字节。协调器采样 RSS 约 2.44–2.50 MiB，采样最高 6 个线程/13 个 FD，未随 20 个排队单元持续增长；采样值不是硬限或精确全进程账本。

### 完整调用开销

同一客体串行运行，每组每轮 40 个独立单元、3 轮，交替对照顺序并核对输出；计时期间没有 Cargo、数据库或另一组负载。包含新进程启动、程序工作、配额/队列、输出 Base64/JSONL、实际 owner 与 cgroup 回收；不包含 VM 启动、文件传输或 C 编译。

| 工作负载 | 普通执行 40 次的中位耗时 | supervise 40 次的中位耗时 | 每个单元增加 |
| --- | ---: | ---: | ---: |
| native OpenSSL 分配/触页/释放 8 MiB | 110.96 ms | 345.46 ms | 5.86 ms |
| Ku hello 程序，含解释器读取/检查/执行 | 60.25 ms | 321.97 ms | 6.54 ms |

这个开销是独立程序的完整执行成本。长期 HTTP 服务只启动一个 worker 时，不能把该数值套成每个 HTTP 请求开销；本测试也不提供跨语言 HTTP 吞吐结论。宿主原有较高 swap 压力仍是环境限制。VM 自身另有成本：空闲客体约 42 MiB；启动后宿主 VZ XPC 的一次实测 physical footprint 为 338.1 MiB、生命周期峰值 413.7 MiB，另有约 75.9 MiB hostagent。它们不属于 worker 的 64 MiB，512 MiB 客体配置也不是同额的宿主 RSS 限制。

本轮未实现逐 Task 完整堆内存计费、逐 HTTP 请求进程隔离、协调器 RSS 的 OS 硬限、自动 DB 重试或自动 VM 桥接。完整 M:N/通用网络 readiness、历史 HTTP 504 与吞吐根因、完整平台实库矩阵和版本发布仍有后续工作；旧 HTTP 正式负载失败没有因本轮资源回归通过而改成通过。此前暂停的通用值复制/释放与 AST 后续分析保持暂停；JSON 继续使用旧默认实现。
