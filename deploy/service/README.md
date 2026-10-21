# 常驻服务恢复与参考部署

`ku supervise` 继续执行有限批次、互不重试的完整程序。新增 `ku service` 管理常驻 native HTTP 实例；Ku 是该业务进程唯一的重启负责人，宿主服务管理器只管理 Ku supervisor。数据库操作和失败的 HTTP 请求均不重放。

## 实际进程与探针

每个实例包含 Ku supervisor、一个小型 Ku launcher 和真实 native 服务进程。`/status` 的 `pid` 是 launcher 的进程组负责人，不是某个 HTTP Task，也不是 native 服务 PID。launcher 只创建一次服务，绝不重复业务初始化；一条保留的 stdin lifetime pipe 在 supervisor 消失时终止真实服务。Windows 另有 kill-on-close Job 包住整组。POSIX 普通模式包含同一进程组，故意脱离该组的任意外部程序应由宿主 cgroup/launchd 管理，不把进程组当作隔离任意特权程序的容器。

启动后的代次使用新的随机 256-bit token。native runtime 只在 `KU_SERVICE_CONTROL_TOKEN` 存在且请求头 `X-Ku-Control-Token` 正确时启用下列管理路径：

| 请求 | 作用 |
| --- | --- |
| `GET /__ku/health/startup` | 必要事件循环已启动 |
| `GET /__ku/health/ready` | 此刻允许新业务；drain 后失败 |
| `GET /__ku/health/live` | 实际循环推进正常，不由独立永远回 OK 的线程伪造 |
| `POST /__ku/drain` | 停止新业务准入，推进存量并退出 |

成功探针 body 必须等于本代 token，supervisor 因此不会把旧实例或碰巧占用同一端口的程序认作新实例。token 不出现在 supervisor 的状态、指标标签和部署结果中。supervisor 把 `--probe-addr` 作为 `KU_SERVICE_PROBE_ADDR` 注入 native 服务：它是独立于业务监听的 loopback 管理端口，不消耗业务 active/pending 槽位；各事件循环的实际 heartbeat 决定健康结果。探针连接、响应读取使用一份绝对期限，响应最多 8192 字节。

Ku supervisor 的独立 loopback 控制口需要 token 文件中的另一个固定部署 token：`GET /status` 返回有界当前状态，`POST /stop` 发起停止。没有任意命令执行或无界历史记录。控制请求最多 4096 字节，每轮只处理一个请求且绝对读取期限最多 200ms。HAProxy 从该控制口读取实际探测后的 readiness，业务仍发往 native HTTP 端口。

## 直接启动

使用 `ku service --help` 查看真实参数。普通服务管理在 Linux、Windows、macOS 均可用；这条命令不默认承诺 OS 内存硬限制。

```sh
python3 -c 'import pathlib,secrets; p=pathlib.Path("service.token"); p.write_text(secrets.token_hex(32)); p.chmod(0o600)'
ku service --probe-addr 127.0.0.1:18181 \
  --control-addr 127.0.0.1:19081 --control-token-file service.token \
  --startup-timeout-ms 5000 --probe-timeout-ms 250 --probe-interval-ms 1000 \
  --failure-threshold 3 --drain-timeout-ms 10000 \
  --restart-limit 5 --restart-window-ms 60000 \
  --backoff-ms 100 --max-backoff-ms 5000 -- ./service
```

启动期限默认 5 秒，探针 250ms/每秒，连续 3 次 live 失败才退役；单独 ready 失败只摘流，不因数据库瞬时不可用重启全部实例。正常意外退出也视为常驻实例失去，按同一受控策略替换。默认一分钟最多 5 次替换，指数退避 100ms 至 5 秒；超强度后 supervisor 报错退出。宿主不得另起一套针对 native 进程的重启循环。

停止先令状态变为 draining，再请求 runtime 排空；固定期限届满后按进程组/Job 终止，最多再用 5 秒确认回收。回收无法确认时拒绝启动替代实例，不提前返还预算。强制终止可能跳过 drop；已经发送的数据库写入结果可能未知，不能据此宣称回滚。

`last_exit_generation`、`last_exit_code`、`last_launcher_signal` 和 `launcher_status` 保留最近一次退休的实际 launcher 记录，替代实例 ready 后也能核对；当前实例由 `generation`/`pid`/`phase` 区分。launcher 保留 native 正常退出码；native 被信号终止后 launcher 可能转换为退出码 1，不能将它冒称为 native 原始 signal。`last_launcher_signal` 只描述实际观察到的 launcher 信号。

| native runtime 约定退出码 | `launcher_status` | 含义 |
| --- | --- | --- |
| 0 | `normal_exit` | 观察到正常退出，常驻实例意外退出仍需替换 |
| 75 | `runtime_unfinished_work` | 期限届满，可能仍有原生工作或在途操作；停止旧实例后才允许替代，不重放旧请求 |
| 76 / 77 | `runtime_join_failure` / `runtime_budget_remaining` | 线程回收失败 / 退休后资源账本仍未归零 |
| 78 / 79 / 80 | `runtime_invariant_failure` / `runtime_metrics_invariant_failure` / `runtime_task_retirement_failure` | 对应运行时不变量失败，按进程替换，不继续复用受影响状态 |
| 其他非零 | `nonzero_exit` | 保留退出码，不猜测异常原因 |

`termination_requested=true` 表示 supervisor 在排空期限后确实发出了进程终止动作；`launcher_status=termination_requested` 同时保留观察到的退出码/launcher 信号，不断言一个竞争中的自然退出必由该动作造成。异常、live 失败或强制终止时 `unfinished_work_possible=true`、`database_outcome=unknown`，表示数据库副作用未确认；`not_observed` 同样不是“已回滚”。`database_retry` 永远 false。OS provider 有真实 OOM/限额数据时优先据此记录原因，普通非零退出码不能替代这些证据。

业务 stdout/stderr 默认丢向系统空设备，避免一个未读取的日志管道阻塞服务。此模式应使用有界 HTTP 指标与管理状态；这不等于承诺保留业务日志。若业务需要持久日志，应使用已明确有界的宿主输出设施；当前 `ku service` 没有隐含无限日志文件。

## 可选 OS 限制

显式同时给出 `--worker-memory-bytes` 和 `--total-memory-bytes` 才进入硬限制模式；Linux 还必须给出现有要求的 `--cgroup-root`，Windows 不接受此参数。完全复用原 provider：Linux cgroup v2 的 memory/pids 委托和计费量、Windows Job 提交内存，指标含义不同；macOS 在运行应用前明确拒绝此模式，绝不换成 RSS 采样退出。详见 [进程执行说明](../../docs/process-execution.md)。

受限域包含 launcher 和 native 服务及后代；supervisor 自身在域外。状态只在有 provider 数据时报告 `limit_metric`、`memory_peak_bytes`、`memory_limit_observed`，未知为 null。Windows 内存提交失败未必杀进程，也不能仅凭一个退出码断言 OOM。

## 两个活动实例与一个滚动备用位

复制并填写 `cluster.example.json`，使用真实 Ku、二进制、token 和代理 socket 路径。每个 slot 的 `listen_addr` 是业务端口、`probe_addr` 是 runtime 管理端口、`control_addr` 是 supervisor 控制端口，所有端口互不冲突。正常运行 a/b，spare 留空。`http_service.ku` 是实际可编译示例：每个 slot 目录有二进制及 `http.addr`（无换行，内容为该 slot 的 `listen_addr`）；使用 `ku build --native service.ku -o service`，从源/输出同目录构建，移动时保留已有 fs 相对路径约定。

部署总预算包含新旧共存的三个实例，不按平时两个实例虚报：示例最多 12 事件线程、12 阻塞线程、53 个已列明线程份额（含一个代理线程与一次管理操作余量）、24 个数据库连接；数据库等待数和字节也按三个实例合计。每个实例声明 512MiB 预留，其中 supervisor 控制余量 32MiB。该预留是部署配置的准入账本，不是 coordinator 全堆精确计量或 macOS 硬上限。

`manage.py` 校验总和，并按每实例预算注入现有真实环境字段：`KU_EVENT_LOOPS`、`KU_BLOCKING_THREADS`、`KU_THREAD_BUDGET`、`KU_THREAD_STACK_BYTES`、`KU_DB_MAX_CONNECTIONS`、`KU_DB_MAX_WAITERS`、`KU_DB_MAX_PENDING_BYTES`。与 slot 环境冲突就报错。线程账本还预留 native main/logger、Ku supervisor/launcher 和可选宿主 lifetime reader 以及 Windows 包装器的 9 个线程；默认 Ku CLI 辅助栈为 8MiB、两个 lifetime reader 各为 64KiB，native 辅助栈使用配置的 2MiB。总进程成本以验收实测为准，不能把栈预留当成 RSS。

参考业务程序只有同步 handler，示例为每实例 4 个事件循环和 4 个阻塞线程。混合 native async 文件/网络调用会从阻塞总量预留 Task 设施线程，N=4 时至少配置总阻塞线程 5，并同步把部署三实例总数与 `total_threads` 提高；不满足时 Ku 明确拒绝启动。A14 入口固定采用 N=4/B=5。

示例 HTTP 的连接/活动请求/等待请求固定为 256/64/128；不因为事件循环数增大而放大。存在其他业务、数据库池或依赖时必须把其真实配置纳入预算，不能仅修改 JSON 中的声明。显式 worker 硬限额加 supervisor 余量不能超过每实例预留。多个 supervisor 没有共享内核域时，部署总量靠该唯一管理入口及宿主限额，不能说是全机绝对封顶。

```sh
python3 manage.py check /etc/ku/cluster.json
python3 manage.py render /etc/ku/cluster.json > /etc/ku/haproxy.cfg
chmod 600 /etc/ku/haproxy.cfg
haproxy -c -f /etc/ku/haproxy.cfg
haproxy -db -f /etc/ku/haproxy.cfg
python3 manage.py start /etc/ku/cluster.json a
python3 manage.py start /etc/ku/cluster.json b
python3 manage.py status /etc/ku/cluster.json
python3 manage.py roll /etc/ku/cluster.json a spare --program /opt/ku/releases/new/spare/service
python3 manage.py rollback /etc/ku/cluster.json
python3 manage.py stop /etc/ku/cluster.json a
python3 manage.py stop /etc/ku/cluster.json b
```

更新保存上一份二进制与兼容配置：先在空 slot 启动新程序，等待 Ku ready **且 HAProxy 实际将新 server 标为 UP**，随后旧 server 进入 maintenance、drain 和退出。回滚同样先恢复旧程序接流，才退役新程序。回滚所需路径必须继续存在；脚本不覆盖任何旧二进制。一次 GET smoke 只验证配置中明确的无副作用路径，不重试业务写入。新程序启动失败时保留旧实例；摘旧后发现的问题保留现存服务并报错，不能伪造事务回滚。

所有有状态操作持有排他部署锁；进程意外退出遗留 `.lock` 时应先核对锁中的 PID 与当前状态，再人工清除，不能在更新结果未知时自动抢锁。`start` 不配置宿主命令时仅用于本机验收，不接管 supervisor 崩溃重启。

控制读取超时或有界读头返回 400 记为 `unreachable`，继续占部署份额，不能据此声称进程已经退出；只有连接明确拒绝才记为 `absent`。检查和停止在原操作期限内重复幂等管理请求，期限不会续期，不重复 HTTP 业务请求或 SQL。

## 宿主与平台

Linux 采用 `ku-service@.service`：只自动恢复异常崩溃的 supervisor，5 秒退避、5 分钟最多 3 次宿主启动，`KillMode=control-group`，停止预算 16 秒。Ku 因自身重启强度耗尽而正常退出码 1 不会被宿主无限再起。`manager_start`/`manager_stop` 示例由 systemd 管理，需按实际安装位置设置服务用户和文件权限。不要在 native 业务进程外再放另一个独立重启器。

macOS 的 `com.ku.service-a.plist` 提供普通模式 launchd 模板；另两份分别改 Label/slot。仅异常崩溃重起、30 秒节流、保留默认进程组清理。需将 JSON 的 manager 命令改为本机实际 `launchctl bootstrap/bootout` 操作，或在隔离验证中移除 manager 字段直接 start/stop。未提供硬内存限制的替代品。

Windows 原生 `ku service` 使用 Job containment，永久 Rust 回归验证普通生命周期。`install-windows-task.ps1 -Config <cluster.json> -Python <python.exe>` 先输出安装计划，加 `-Register` 才注册 Task Scheduler；默认以 Local Service 执行，需让该账户能读取程序、配置和 token，或显式传入已配置的服务账户。a/b 带开机触发器，spare 只在更新时启动；宿主最多重启 3 次、间隔 60 秒（遵循 [Task Scheduler 最小一分钟](https://learn.microsoft.com/en-us/windows/win32/taskschd/taskschedulerschema-interval-restarttype-element)）。默认名称 `Ku-Service-a`，用 `-TaskPrefix` 可指定独有前缀。把 slot 的 manager 命令替换为 `powershell.exe -NoProfile -Command "Start-ScheduledTask -TaskName Ku-Service-a"`/`Stop-ScheduledTask` 等对应命令。包装器在 Windows 为 Ku supervisor 启用 `--watch-stdin`，宿主包装器退出/被终止导致管道 EOF，走同一 drain/期限停止策略；强制杀 supervisor 时其 native Job 关闭回收业务进程。不要给 native 进程另注册任务。下述 CI 入口执行真实安装和停止；macOS 本机不能给出 Windows 已通过结论，必须查看对应 runner 的结果。

Windows 的外部代理可使用另一台 Linux/macOS 宿主上的成熟代理；本仓库不会把 Unix HAProxy Runtime socket 管理脚本称作 Windows 原生代理安装器。普通 native HTTP 与 Task/Job 生命周期仍由三平台 CI 覆盖。

参考代理配置明确 `retries 0`、关闭 redispatch，删除外来控制 token 并拒绝 `/__ku/`。示例仅在 loopback HTTP 上运行，不擅自部署到公网；生产可由既有成熟入口终止 TLS，本次没有实现新 TLS 协议。

## 多主机边界

`haproxy.multihost.example.cfg` 是外层入口：两个授权私网宿主各运行上述本地两实例+备用位，外层只检查一个明确无副作用的健康路径。实际部署应把本机入口绑定为受访问控制的私网地址；loopback 示例不会自动变成远程服务。每个故障域都要分别测摘除、剩余容量和客户端恢复。

该示例的外层 HAProxy 和数据库仍可能是单点；需要现有 VIP/负载入口和成熟 PostgreSQL 主备/备份方案。Ku 不选主、不重放 SQL。RTO/RPO 由实际入口切换、数据库复制与备份恢复测得，配置两个地址不能证明零丢失。没有授权双宿主及数据层环境时，跨主机演练明确未执行。

## 可复现验收入口

```sh
python3 test_manage.py
cargo test --test service_supervision_test
python3 verify.py --ku /absolute/path/ku --haproxy /absolute/path/haproxy --output /absolute/path/service-results.json
python3 verify.py --blocking-only --ku /absolute/path/ku --output /absolute/path/native-blocking-retirement.json
# Linux 需要已经提供的独有空 cgroup v2 委托；Windows/macOS 不传 cgroup-root。
python3 verify.py --memory-only --ku /absolute/path/ku --cgroup-root /absolute/delegated/cgroup --output /absolute/path/memory-provider.json
# Windows 临时 CI runner，必须有 Task Scheduler 注册权限：
python verify.py --blocking-only --windows-host-task --ku C:\\absolute\\ku.exe --output C:\\absolute\\host-task.json
```

`verify.py` 在 `/tmp` 私有目录构建真实 Ku native 两个修订，运行三个 slot 与 HAProxy，验证业务进程退出替换、另一实例继续服务、滚动接流、回滚内容及清理。普通启动 5 秒，替代 ready 要求 10 秒内，单次管理操作 15 秒；会保留失败摘要，不把故障注入称为正常稳态。源/产物/配置/token/进程自动清理，仅保留指定 JSON。此入口只覆盖服务恢复参考链路，不代替 HTTP→PG、资源矩阵和正式 30 分钟整体验收。

`--blocking-only` 复用 `tests/acceptance/runtime_matrix.py` 的私有 FIFO/Windows named pipe；真实 Ku 的 `fs.read()` 在内核等待，HTTP 1,000ms 后 504，原生工作继续持有，清理 1,000ms 后进入实例排空，200ms 后按 runtime 约定退出 75。supervisor 在真正回收后替代，另一实例接下来的 10 次请求必须正确，10 秒内新实例必须可服务。没有数据库，也没有释放管道来伪造旧工作自然完成。macOS 2026-10-21 实测进入等待 102.6ms、504 1,001.3ms、摘 ready 2,135.5ms、替代可服务 2,580.3ms，退出 75、另一实例 10/10 正确、清理无错误；仅证明该受控故障条件。

`--windows-host-task` 仍执行同一真实 native 等待和替代，再调用真实 `Stop-ScheduledTask` 停止宿主包装器，核对另一实例继续响应，最后停止/注销全部独有任务并检查该临时树的剩余进程。它使用内置 Local Service 账户，所有脚本、配置和产物复制到私有临时树后仅对该树授予账户读/执行；不读取真实数据库、外部凭据或修改已有任务。注册 API 拒绝、停止失败或进程残留会使入口失败并留下错误摘要，不把语法检查当作宿主验收。三平台实际结果以本次 CI 归档为准。

`--memory-only` 只执行一组本次 `ku service` 接线检查，不重跑既有 36 次矩阵。固定 N=2/B=2，每个受控实例 64MiB，真实同步 handler 从 64KiB 字符串倍增请求分配至 256MiB；要求受限旧实例失败并回收、15 秒内替代实例正常响应、另一实例 10 次后续请求正确。Linux 必须有本机独有空 cgroup v2 委托且已启用 memory/pids，必须观察到真实限额/OOM 指标，并在停止后确认委托下没有遗留域。Windows 报告 Job 提交量指标及实际退出；提交失败可能通过库分配失败退出，不能仅凭退出码称作 OS 杀进程或 OOM。macOS 只验证实际硬模式启动前拒绝且没有执行应用；2026-10-21 本机该拒绝检查通过。Linux/Windows 新服务硬模式结果在实际 CI 运行后确认，不能由 macOS 拒绝结果替代。

设计参考：[OTP 重启强度](https://www.erlang.org/doc/system/sup_princ.html)、[探针职责](https://kubernetes.io/docs/tasks/configure-pod-container/configure-liveness-readiness-startup-probes/)、[HAProxy 重试与 redispatch](https://www.haproxy.com/documentation/haproxy-configuration-tutorials/reliability/retries/)。实际实现采用仓库已有 OS provider 和安装的 HAProxy 语法，来源不代表对 Ku 的认证。
