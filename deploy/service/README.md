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

复制并填写 `cluster.example.json`，使用真实 Ku、二进制、token 和代理 socket 路径。每个 slot 的 `listen_addr` 是业务端口、`probe_addr` 是 runtime 管理端口、`control_addr` 是 supervisor 控制端口，所有端口互不冲突。正常运行 a/b，spare 留空。将 `http_service.ku` 复制为各 slot 的 `service.ku`；各 slot 使用独立目录，例如 `/opt/ku/releases/current/a/` 和 `/opt/ku/releases/current/b/`，目录内准备 `http.addr`（无换行，内容为该 slot 的 `listen_addr`）。在各自目录使用 `ku build --native service.ku -o service` 构建，二进制与地址文件放在一起；移动时保留已有 fs 相对路径约定。不能把读取同名 `http.addr` 的不同 slot 放在同一目录。

部署总预算包含新旧共存的三个实例，不按平时两个实例虚报：Unix原示例最多 12 事件线程、12 阻塞线程、53 个已列明线程份额（含一个代理线程与一次管理操作余量）、24 个数据库连接；数据库等待数和字节也按三个实例合计。每个实例声明 512MiB 预留，其中 supervisor 控制余量 32MiB。该预留是部署配置的准入账本，不是 coordinator 全堆精确计量或 macOS 硬上限。

`manage.py` 校验总和，并按每实例预算注入现有真实环境字段：`KU_EVENT_LOOPS`、`KU_BLOCKING_THREADS`、`KU_THREAD_BUDGET`、`KU_THREAD_STACK_BYTES`、`KU_DB_MAX_CONNECTIONS`、`KU_DB_MAX_WAITERS`、`KU_DB_MAX_PENDING_BYTES`。与 slot 环境冲突就报错。线程账本还预留 native main/logger、Ku supervisor/launcher 和可选宿主 lifetime reader 以及 Windows 包装器的 9 个线程；默认 Ku CLI 辅助栈为 8MiB、两个 lifetime reader 各为 64KiB，native 辅助栈使用配置的 2MiB。KU_THREAD_STACK_BYTES只设置Ku创建的对应线程，不控制OS或第三方辅助线程的栈。总进程成本以验收实测和可用OS限额为准，不能把栈预留当成 RSS。

参考业务程序只有同步 handler，示例为每实例 4 个事件循环和 4 个阻塞线程。混合 native async 文件/网络调用会从阻塞总量预留 Task 设施线程，N=4 时至少配置总阻塞线程 5，并同步把部署三实例总数与 `total_threads` 提高；不满足时 Ku 明确拒绝启动。A14 入口固定采用 N=4/B=5。

示例 HTTP 的连接/活动请求/等待请求固定为 256/64/128；不因为事件循环数增大而放大。存在其他业务、数据库池或依赖时必须把其真实配置纳入预算，不能仅修改 JSON 中的声明。显式 worker 硬限额加 supervisor 余量不能超过每实例预留。多个 supervisor 没有共享内核域时，部署总量靠该唯一管理入口及宿主限额，不能说是全机绝对封顶。

```sh
python3 manage.py check /etc/ku/cluster.json
python3 manage.py render /etc/ku/cluster.json > /etc/ku/haproxy.cfg
chmod 600 /etc/ku/haproxy.cfg
haproxy -c -f /etc/ku/haproxy.cfg
# 终端 A：代理在前台持续运行。
haproxy -db -f /etc/ku/haproxy.cfg
```

保持代理运行，在另一个终端执行管理命令：

```sh
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

控制读取超时或有界读头返回 400 记为 `unreachable`，继续占部署份额，不能据此声称进程已经退出；连接明确拒绝才记为 `absent`；Windows 的 connect 错误还可在同一确切 loopback 控制端口上用 `SO_EXCLUSIVEADDRUSE` 独占绑定证明确实空闲，绑定失败仍保留不可达及预算，自有存活 supervisor 未监听时也不算空闲。检查和停止在原操作期限内重复幂等管理请求，期限不会续期，不重复 HTTP 业务请求或 SQL。

## 宿主与平台

Linux 采用 `ku-service@.service`：只自动恢复异常崩溃的 supervisor，5 秒退避、5 分钟最多 3 次宿主启动，`KillMode=control-group`，停止预算 16 秒。Ku 因自身重启强度耗尽而正常退出码 1 不会被宿主无限再起。`manager_start`/`manager_stop` 示例由 systemd 管理，需按实际安装位置设置服务用户和文件权限。不要在 native 业务进程外再放另一个独立重启器。

macOS 的 `com.ku.service-a.plist` 提供普通模式 launchd 模板；另两份分别改 Label/slot。仅异常崩溃重起、30 秒节流、保留默认进程组清理。需将 JSON 的 manager 命令改为本机实际 `launchctl bootstrap/bootout` 操作，或在隔离验证中移除 manager 字段直接 start/stop。未提供硬内存限制的替代品。

Windows 普通三进程实例另需显式填写 `deployment_budget.per_instance.platform_thread_reserve: 16`，`per_instance.total_threads` 至少为 `event_threads + blocking_threads + 9 + platform_thread_reserve`；4循环/4后台即至少33，原HAProxy型管理预留2时部署总量至少101，Caddy管理34时至少133。缺失、为负或预算不足在启动前报错。这是依据本轮实际每实例24线程（协调器6、launcher6、native12）修正原17份额漏算，并留平台辅助余量；额外7线程的具体来源未唯一归属，16不冒充系统线程的硬上界。该预留不增加执行线程，native的KU_THREAD_BUDGET仍为event+blocking+2。Task Scheduler另有Python包装器；下方可运行配置为它再预留16份线程，每实例采用49、Caddy部署总181，不能只算三进程最低33。额外预留不创建新线程，实际平台仍应核查进程成本。

Windows 原生 `ku service` 使用 Job containment，永久 Rust 回归验证普通生命周期。`install-windows-task.ps1 -Config <cluster.json> -Python <python.exe>` 先输出安装计划，加 `-Register` 才注册 Task Scheduler；默认以 Local Service 执行，需让该账户能读取程序、配置和 token，或显式传入已配置的服务账户。a/b 带开机触发器，spare 只在更新时启动；宿主最多重启 3 次、间隔 60 秒（遵循 [Task Scheduler 最小一分钟](https://learn.microsoft.com/en-us/windows/win32/taskschd/taskschedulerschema-interval-restarttype-element)）。默认名称 `Ku-Service-a`，用 `-TaskPrefix` 可指定独有前缀。把 slot 的 manager 命令替换为 `powershell.exe -NoProfile -Command "Start-ScheduledTask -TaskName Ku-Service-a"`/`Stop-ScheduledTask` 等对应命令。包装器在 Windows 为 Ku supervisor 启用 `--watch-stdin`，宿主包装器退出/被终止导致管道 EOF，走同一 drain/期限停止策略；强制杀 supervisor 时其 native Job 关闭回收业务进程。不要给 native 进程另注册任务。下述 CI 入口执行真实安装和停止；macOS 本机不能给出 Windows 已通过结论，必须查看对应 runner 的结果。

Windows 的本机参考代理使用官方原生 Caddy 2.11.7，Linux/macOS原有HAProxy方式不变。Caddy不进入Ku编译器或产物依赖；只有选用该部署方式时才需要它。固定版本下载及校验见[官方Release](https://github.com/caddyserver/caddy/releases/tag/v2.11.7)，压缩包内保留Apache-2.0许可证。Windows amd64 ZIP的SHA-256为 `0a1edc0b799512051c57071ce0e798d3f2cf67dc98171366f1d2b326072e3b06`。

从原cluster配置填写Windows实际程序、token和Task Scheduler路径，增加下列字段，并修改现有deployment_budget对应字段（保留其余预算和三个slots）：

```json
{
  "proxy_kind": "caddy",
  "caddy_admin_addr": "127.0.0.1:19100",
  "caddy_health_addr": "127.0.0.1:19101",
  "deployment_budget": {
    "management_threads": 34,
    "management_memory_bytes": 134217728,
    "total_threads": 181,
    "reserved_memory_bytes": 1744830464,
    "per_instance": {
      "platform_thread_reserve": 16,
      "total_threads": 49
    }
  }
}
```

这段是覆盖字段，不能拿它替代完整cluster.json。数字对应Windows带Task Scheduler包装器的参考预留每实例49线程、512MiB、最多三实例；移除宿主包装器直接管理时最低为33，总133；若调整实例配置，管理额外预算必须重新相加。Caddy预留32线程/128MiB和两个管理/输出采集线程，平台辅助余量另放在每实例账本；这些属于部署准入预留，并非Go或OS线程/内存硬限制；Ku服务自身的OS保证仍按前文。验收保留代理实际进程成本，超过声明预留会失败。`GOMAXPROCS=1`只限制Go并行度，不能称为单OS线程。

在deploy/service目录用Python写出无BOM配置，随后启动固定Caddy；另一个终端沿用同一个manage.py的start/status/roll/rollback/stop命令。Windows安装示例不能直接照搬systemctl字段，须先按上节替换manager_start/manager_stop。

```powershell
python -B manage.py check C:\ku\cluster.json
python -B -c "import manage,sys; from pathlib import Path; Path(sys.argv[2]).write_text(manage.render(manage.load(Path(sys.argv[1]))), encoding='utf-8')" C:\ku\cluster.json C:\ku\caddy.json
$env:GOMAXPROCS = '1'
C:\ku\caddy.exe validate --config C:\ku\caddy.json
C:\ku\caddy.exe run --config C:\ku\caddy.json
```

渲染配置含控制token，应只让部署/代理服务账户读取。admin与健康路由必须是互不冲突的loopback端口，不能公开；入口拒绝`/__ku`，删除外来控制header。Caddy内部固定健康路由将各slot对应到自己的supervisor/status，业务转发只到native地址，不重放HTTP/SQL；关闭Go上游keep-alive以避免其连接级自动重试。更新经本机有界Admin API原子load，拒绝覆盖与当前配置不一致的代理。新实例需经过实际active health报告才摘除旧实例；该指标最多约十秒刷新一次，沿用原操作期限，初始乐观UP不算通过。接口依据见[Caddy Admin API](https://caddyserver.com/docs/api)和[reverse_proxy健康检查及重试](https://caddyserver.com/docs/caddyfile/directives/reverse_proxy)。

Windows使用实际原生Caddy的定向验收命令为：

```powershell
python -B deploy/service/verify.py --ku C:\ku\ku.exe --caddy C:\ku\caddy.exe --output C:\ku\caddy-result.json
```

该入口验证真实原生进程退出替代、旧/新版本、代理更新和回滚、资源与清理，不能用本机macOS运行结果替代Windows runner结果。

参考代理配置明确 `retries 0`、关闭 redispatch，删除外来控制 token 并拒绝 `/__ku/`。示例仅在 loopback HTTP 上运行，不擅自部署到公网；生产可由既有成熟入口终止 TLS，本次没有实现新 TLS 协议。

## 多主机边界

以下是隔离测试环境的操作流程，**尚未执行 A19**。需要两台明确授权、可停止并恢复的专用测试宿主，独立测试入口和客户端；数据库演练另需独有测试库及已有 PostgreSQL 主备/备份方案。不得将生产地址、共享数据库或已有生产宿主代入。开始前记录宿主/实例标识、私网地址、提交和产物摘要、入口切换命令、测试虚拟机停止/启动命令、数据库故障/恢复命令及负责人；任一项无法确认就保留对应外部阻塞，不执行故障动作。本文不提供通用远程关机或数据库选主命令。

拓扑为客户端 → 独立外层入口 → 测试宿主 A/B 的本地 HAProxy → 各自 a/b 两个活动实例，spare 只供滚动共存。`haproxy.multihost.example.cfg` 的 `10.0.0.11:18080`、`10.0.0.12:18080` 必须替换为这两台测试宿主的实际私网入口；将各自 `cluster.json` 的 `frontend_addr` 从 loopback 改为对应私网地址，并限制来源为测试外层入口。slot 的业务/探针/控制端口仍保持 `127.0.0.1`，不能把管理口直接暴露到私网。外层配置默认仅绑定 `127.0.0.1:18000`；客户端同机时原样使用，远程客户端则仅在受访问控制的测试地址监听。

每台保留完整三实例预算：示例为 1.5 GiB 声明预留、12 事件线程、12 阻塞线程、53 个已列明线程份额、24 个数据库连接；两台总和为 3 GiB、24/24/106 线程份额和 48 个数据库连接，另加外层入口、客户端和数据库成本。这不是全系统硬限额。普通示例不连接数据库，但替换成真实 DB 应用时连接总预算不能随故障翻倍；剩余一台必须独立承受预定到达率，否则先确定并记录允许的拒绝比例，不能以双机名义承诺完整容量。

在每台测试宿主的仓库 `deploy/service` 目录，按上文准备各 slot 的地址文件、二进制、token 和宿主管理配置后执行；以下 `/etc/ku-test/` 均为本次独有目录：

```sh
python3 manage.py check /etc/ku-test/cluster.json
python3 manage.py render /etc/ku-test/cluster.json > /etc/ku-test/haproxy.cfg
chmod 600 /etc/ku-test/haproxy.cfg
haproxy -c -f /etc/ku-test/haproxy.cfg
# 独立终端前台运行，每台仅一份；保留该终端用于最后 Ctrl-C 停止。
haproxy -db -f /etc/ku-test/haproxy.cfg
# 另一终端启动实例并确认两项 ready。
python3 manage.py start /etc/ku-test/cluster.json a
python3 manage.py start /etc/ku-test/cluster.json b
python3 manage.py status /etc/ku-test/cluster.json
```

在独立测试入口机复制并填写外层配置后检查、启动，另一个终端先分别请求两台私网 `:18080/`，再请求外层 `:18000/`；三者都必须为 `200` 和精确 body `Ku HTTP OK`，才开始故障流程：

```sh
haproxy -c -f /etc/ku-test/haproxy.multihost.cfg
haproxy -db -f /etc/ku-test/haproxy.multihost.cfg
```

唯一连续业务样本为无副作用 `GET /`，每次使用新请求 ID，不重发失败请求。下列标准库客户端在**独立客户端机**执行；先将环境变量 `KU_HA_TEST_ENTRY_HOST` 设置为已确认的固定私有入口/VIP地址，不能随故障切换改成存活宿主地址。客户端按 360 秒/720 次上界发起，每条记录含本机单调时钟、UTC、ID、状态/错误和耗时。响应最多读 128 字节，socket 连接/读取超时各一秒；它是 2 次/秒的连续恢复探测，不能代替容量验收。需要证明剩余容量时，必须同时使用预先冻结的真实业务负载，并保留其全部分母。

```sh
python3 -u - <<'PY' > /etc/ku-test/client.jsonl
import http.client, json, os, time
entry_host = os.environ["KU_HA_TEST_ENTRY_HOST"]
origin, end = time.monotonic_ns(), time.monotonic() + 360
for index in range(720):
    if time.monotonic() >= end:
        break
    started = time.monotonic_ns()
    record = {"id": f"ku-ha-test-{index}", "utc_ns": time.time_ns(), "start_ns": started}
    connection = http.client.HTTPConnection(entry_host, 18000, timeout=1)
    try:
        connection.request("GET", "/", headers={"X-Request-ID": record["id"], "Connection": "close"})
        response = connection.getresponse()
        body = response.read(128)
        record.update(status=response.status, correct=response.status == 200 and body == b"Ku HTTP OK")
    except (OSError, http.client.HTTPException) as error:
        record.update(correct=False, error=type(error).__name__, detail=str(error)[:160])
    finally:
        connection.close()
    record.update(elapsed_ms=(time.monotonic_ns() - started) / 1e6, offset_ms=(started - origin) / 1e6)
    print(json.dumps(record), flush=True)
    time.sleep(max(0, .5 - (time.monotonic_ns() - started) / 1e9))
PY
```

每次只注入一个故障，至少先取得 20 条连续正确样本。动作前后在**同一客户端机**记录 `time.monotonic_ns()` 和 `time.time_ns()`，保存命令退出码/平台事件和两台 `manage.py status`，不以跨机未校准的时钟直接相减。RTO 起点取客户端发起故障动作前的时间，注明其中包含控制命令传递时间；实际故障时刻只能定位在动作前后区间时，同时保留该不确定范围。故障后以首次连续 10 条正确响应为恢复确认，结束点取这 10 条中的第一条；单独保留故障期间全部错误，不删除短暂失败。预先冻结宿主/入口恢复目标各 10 秒、数据库恢复目标 30 秒；设施无法满足时应在故障前写明实际目标和依据，执行后不得续期以改判通过。

1. **测试宿主停止。** 在已确认的测试虚拟化平台停止 A 的专用测试虚拟机，记录实际停止事件；不先通过 `manage.py stop` 排空来冒充宿主突然消失。核对外层摘除 A，B 的本地两项仍 ready，客户端在上述预算内恢复；保存外层到 A/B 的状态或平台连通性记录，区分探针摘除与请求恢复。用计划中的启动动作恢复 A，确认本地 a/b ready、直接私网请求正确、外层重新接入后再测试 B。只能停止本机实例时，结果记为进程级覆盖，不能标为宿主故障通过。
2. **入口切换。** 外层 HAProxy 本身仍是单点。只有已有第二个测试入口及授权 VIP/负载入口切换能力时，先在备用入口用同一配置检查并验证直连，再在测试平台停用当前入口并执行预先记录的切换动作；客户端始终请求同一个测试入口地址，不能改 URL 伪造恢复。分别记录故障、入口切换和首次持续正确响应。恢复旧入口前避免两个入口同时争用 VIP；缺少这套设施则该故障域未验证。
3. **隔离数据库故障与提交未知。** 上面的普通 HTTP 示例没有 DB，不能据此声称数据库恢复通过。最小本机依赖故障复现入口为仓库根目录的 `python3 -B tests/acceptance/http_pg_lifecycle.py --ku /absolute/path/ku --loops 4 --output /absolute/private-test-dir/pg.json`；它创建并清理独有 PG 集群、表和故障代理，覆盖慢查询、后端终止及已写入但确认丢失。跨宿主 DB 演练必须另行准备两台应用共用的独有测试库/主备入口、测试账号和已确认的故障恢复命令；使用现有 DB 示例时，为各 slot 准备指向该测试库的 `db.conn` 和独有测试表，外层健康路径改成 `/health`，本地 `cluster.json` 的 `smoke_path`/`smoke_expected` 改为 `/health`/`ready`。该健康路径只返回 `ready`，不执行 SQL，仅确认 HTTP 进程响应。此时客户端必须把路径改成 `/read`，解析 JSON 并逐字段核对 `text` 与 fixture 的中文/emoji/引号/反斜杠参数一致、`number` 为字符串 `"42"`、`absent` 为 true、`rows` 为 1、`cols` 为 3；`error:`、解析失败或任一字段不符均记为错误。原 `GET /`/`Ku HTTP OK` 循环只验普通 HTTP，不能把它或健康 200 当数据库可用证明。按既有 PG 方案依次制造测试依赖不可达、恢复以及确认丢失；隔离夹具的 `/insert/{id}` 每次只发送一笔唯一 ID 的测试写入，它是有副作用的测试专用 GET，不能作为公共 API 或健康检查。记录客户端结果、数据库日志/行值和复制位置；`execution_unknown` 或连接断开均不自动重发，worker 退出也不代表回滚。旧主隔离、选主和备份恢复由既有 PG 方案执行，Ku 不承担这些动作。

RPO 只在主备/备份恢复后的**实际测试库**中核对：分别列出故障前已确认提交、结果未知、恢复后查到和缺失的唯一写入 ID，记录对应时间/LSN；单条未知写入的最终有无不能代表完整 RPO。没有复制/恢复证据就写“未测得”，不从 HTTP 成功推断零丢失。恢复和清理总操作窗口限定 120 秒；超过本次既定恢复目标即记录失败，仍在清理窗口内停止注入、按原计划恢复测试宿主/入口/PG，不无限等待或重复注入。清理无法完成时保留确切资源标识和错误，不能标为全部结束。

最后先停止客户端与故障注入，确认两台服务恢复正常，再分别 `manage.py stop /etc/ku-test/cluster.json a`、`b` 和实际启用的 `spare`；停止本次独有宿主管理单元、两个本地及外层 HAProxy 前台进程，核对测试监听/进程与私有 PG 连接已清空。仅删除本次独有 token、配置副本、临时库和构建产物，保留精简客户端/平台/数据库证据，不能删除用户资料或共享库。缺少授权双宿主、入口切换或数据层设施时，A19 对应部分明确未执行；配置、runbook 和本机依赖验证均不能替代真实跨主机验收。

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

`--memory-only` 只执行一组本次 `ku service` 接线检查，不重跑既有 36 次矩阵。固定 N=2/B=2，每个受控实例 64MiB，真实同步 handler 从 64KiB 字符串倍增请求分配至 256MiB；要求受限旧实例失败并回收、15 秒内替代实例正常响应、另一实例 10 次后续请求正确。Linux 必须有本机独有空 cgroup v2 委托且已启用 memory/pids，必须观察到真实限额/OOM 指标，并在停止后确认委托下没有遗留域。Windows 报告 Job 提交量指标及实际退出；提交失败可能通过库分配失败退出，不能仅凭退出码称作 OS 杀进程或 OOM。macOS 只验证实际硬模式启动前拒绝且没有执行应用；2026-10-21 本机该拒绝检查通过。Linux 冻结源 CI 已实测64MiB私有域限额/叶层回收，替代294.52ms；Windows定向CI已实测64MiB提交限额峰值63,438,848B、替代484ms。两者另一实例均10/10、清理无错误；不由macOS拒绝结果替代。

设计参考：[OTP 重启强度](https://www.erlang.org/doc/system/sup_princ.html)、[探针职责](https://kubernetes.io/docs/tasks/configure-pod-container/configure-liveness-readiness-startup-probes/)、[HAProxy 重试与 redispatch](https://www.haproxy.com/documentation/haproxy-configuration-tutorials/reliability/retries/)。实际实现采用仓库已有 OS provider 和安装的 HAProxy 语法，来源不代表对 Ku 的认证。

最终服务定向CI39705058328（源67d78d9）三平台实际通过，完整身份与历史失败见[主验收记录](../../docs/multiloop-runtime.md)。Linux/macOS实际HAProxy退出替代241.664/730.645ms，滚动523.164/1274.199ms，回滚486.591/1754.614ms；Windows实际LocalService宿主替代2844ms、另一实例10/10，Stop后退出0，三个独有任务全部注销且临时树无进程。三平台真实PG故障与恢复、退出后连接清零通过，全部cleanup_errors=[]。这些单机/CI结果不等于上方A19真实主机故障演练，后者缺授权宿主、入口和隔离数据层。

最终Windows原生代理补项见run39709674234（e40ab1b）：Caddy2.11.7实际退出替代219ms、更新17.031s、回滚15.969s，回滚内容20/20正确。15s操作字段限定各ready/health/retire阶段，整次更新包含多个阶段，不声称其总耗时低于15s。实际共存11进程/88线程，管理16、每slot24，分别在总133/管理34/单slot33的普通三进程预留下；四个协调器退出0、代理显式终止回执1、清理PID空/cleanup空。带Task Scheduler包装器的参考配置另留每实例49/总181，不能将本轮普通三进程采样当作包装器全树采样。此前真实宿主注册/恢复/停止由67d独立验收保留。
