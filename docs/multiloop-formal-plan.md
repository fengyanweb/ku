# 多循环正式 30 分钟验收计划与结果

状态：冻结源 `a31c1135ca82e3e15c426a2ef522522038ea8735` 的正式窗口 [run39699000190](https://github.com/fengyanweb/ku/actions/runs/39699000190) **完整通过**，未改变原参数与门槛。本轮包含删除锁内多余通知的修复；此前d173通过与初轮取消均保留历史身份。后续服务协调器控制连接修复不改变本负载的编译器C发射、native运行时、驱动和fixture；该独立服务管理路径另作定向验收，不把此窗口称为协调器验收。

## 固定负载与门槛

使用 [`multiloop-sustained.yml`](../.github/workflows/multiloop-sustained.yml)，在独立的 `ubuntu-24.04` GitHub runner 上构建被标记 PR 的实际 head 提交。构建命令为 `cargo build --locked --release --bin ku`，完成后才进入计时窗口：

```sh
python3 -u -B tests/acceptance/multiloop_load.py \
  --ku target/release/ku --loops 4 --duration 1800 --rps 50 \
  --profile release --output /absolute/path/report.json
```

复用原驱动和 [`multiloop_service.ku`](../tests/fixtures/multiloop_service.ku)，不改变期限、执行架构或门槛：

| 项目 | 冻结值 |
| --- | --- |
| 运行时 | 4 个事件循环；后台线程总额 5；native 线程预算 16；Task 16 MiB |
| HTTP 额度 | 连接 256；活动 128；排队 128；缓冲 8 MiB |
| 普通到达率 | 50 请求/秒；固定 32 个客户端线程、64 个在途/排队上界 |
| 路由轮转 | 普通响应、CPU5000、25ms timer、CPU5000、CPU200000 |
| 阶段 | warm 60 秒 → steady 1470 秒 → overload 30 秒 → recovery 240 秒，共 1800 秒 |
| 过载 | 150 请求/秒，慢体连接最多 280 条、补连最多 280 次/秒，无追赶突发 |
| 普通阶段 | 非预期错误 0；压测端主动丢弃 0；p99≤250ms |
| 过载前循环延迟 | 累计峰值≤100ms；过载后的累计值不能当作 recovery 独立峰值 |
| 过载与恢复 | 错误 body 0；从实际撤载起 5 秒内正确响应且逻辑队列/Task 回落 |
| 正常收尾 | native drain 退出码 0 |

handler/header/body/write/idle 沿用 fixture 的 1000ms。30 秒过载沿用已冻结的同机临时端口预算，不因换到 Linux 加长、缩短或提高服务期限。失败请求不重发。慢体 peer 的预期拒绝、期限、无状态传输关闭与普通请求错误分开保留，不能把所有连接错误算作预期拒绝。

过载必须实际触发服务端准入拒绝。测量请求中的直接对端 EOF/reset/broken-pipe 单列为服务传输终止，它不等于 HTTP 503，也不能给每次关闭补写唯一原因；压测端的在途/队列额度拒绝另列，全部仍在总到达分母内。仅过载阶段允许这两种已明确来源的终止/拒绝，其他传输、客户端超时、协议错误及非 200/408/503/504 HTTP 响应均失败。正常阶段任何此类错误或发生器拒绝均失败。所有规则在正式执行前固定。

## CPU 与证据边界

计时前记录 `uname`、`lscpu`、`nproc`、Python、C 编译器、Rust/Cargo、affinity、可见 cgroup v2 祖先 CPU quota、PR head SHA、CLI/驱动/fixture SHA-256。affinity、`nproc` 和任何可见祖先 quota 中的最小容量必须≥4，否则明确失败，不减少循环数、不改写配额、不把两核 runner 当四核使用。不能读取和建立 quota 关系时也失败。

这个检查证明 runner 可见的配置容量，不能保证云宿主没有竞争；保留窗口前后 `/proc/stat` 和 CPU pressure 供解释。服务与有界 Python 客户端仍在同一 runner，记录的是完整调用链与同机发生器成本，不宣称服务器独享四个物理核心。该入口不要求关闭本机 Codex。

驱动每 10 秒有界采样 native RSS、线程、FD、队列与运行时指标；报告最终责任人/缓冲回落。摘要列出观测峰值，不能将采样峰值称为连续精确峰值。GNU `time -v` 另记整个调用的累计 CPU、墙钟时间和最大单进程 RSS；它不是同时存活进程树的总内存峰值。

`plan.json` 在任何负载前写入；驱动还会在编译和发流量前写入自己的参数/来源报告。最终 `report.json`、`driver-exit.json`、`summary.json`、编译/驱动日志及环境证据无论成功失败都上传，保留 30 天。只有驱动完整通过且退出 0 才能报告通过；计划文件、任务运行中或仅上传 artifact 都不是通过。

正式步骤最多 35 分钟，整个 job 最多 45 分钟。正常或异常退出优先以 SIGINT 触发现有驱动 `finally` 清理，再按有限宽限升级退出，进程组覆盖本轮编译器与 native 子进程。GitHub runner 被强制销毁等情况可能只有最后一份检查点；没有最终成功记录仍算未通过。

## 触发与提交规则

只监听 `pull_request` 的 `labeled` 活动，且仅标签名 **`run-multiloop-sustained`** 运行 job。提交新代码、保留已有标签或重新打开 PR 都不会自动再次跑 30 分钟。需要新一轮时先确认待测 head，再移除并重新添加该标签；这属于明确启动一轮，不能无限自动重试。并发组不取消正在执行的同 PR 窗口。

工作流仅有 `contents: read`，checkout 禁止持久化凭据，不读取发布密钥，不部署、不发布。显式 `ref: github.event.pull_request.head.sha`，避免把 PR merge SHA 当作被测 head；报告同时保留工作流本身的来源 SHA。

GitHub 官方说明：[`pull_request` 支持 labeled，默认使用 PR merge ref，测试 head 需显式指定 head.sha](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#pull_request)；[该事件的工作流文件也来自 PR merge commit](https://docs.github.com/en/actions/reference/security/securely-using-pull_request_target)。因此新文件可随可合并 PR 提交后触发，不要求先合入默认分支。PR 有冲突、Actions 策略或 fork 审批阻止运行时，保留实际阻塞，不能称为通过，也不改用更高权限事件绕过。

## 本轮实际结果

被测源为干净 `a31c113`；CLI SHA-256 `bd5dc30a225f5dcd2558fe2bcad4c372ab08caa7986f741f9e2f0a5748655be2`，native `96e61062c3e7f9b49dbd8135aef418ee3cb06e8b776cc055ed965cc2120ab389`。Ubuntu24.04、Linux6.17.0-1022-azure、4个有效逻辑CPU（2core×2SMT），GCC13.3、Python3.12.3、Rust1.89；affinity与可见祖先quota核查满足原四CPU门槛。runner总RAM与memory.max本次未采集，报告为未知；本窗口不是OS内存限额验收。

| 阶段 | 到达数 | 实际结果 | p95 / p99 |
| --- | ---: | --- | ---: |
| warm 60s | 3,000 | 3,000正确，错误/发生器拒绝0 | 25.5 / 25.5ms |
| steady 1470s | 73,500 | 73,500正确，错误/发生器拒绝0 | 25.5 / 25.6ms |
| overload 30s | 4,500 | 1,460正确；215 HTTP503；19传输终止；2,806发生器额度拒绝 | 917.8 / 945.2ms |
| recovery 240s | 12,000 | 12,000正确，错误/发生器拒绝0 | 25.5 / 25.6ms |

普通三阶段共 **88,500/88,500正确**。过载保留全部4,500到达分母；19条直接对端传输终止没有补写为503或逐个猜测关闭分支。慢体另计8,099连接：4,163 HTTP503、3,712 HTTP408、224阶段末由发生器关闭。撤载后 **189.279ms** 正确响应且逻辑队列/Task/HTTP字节归零；各循环累计延迟峰值1ms。最终native自然drain退出0。

native线程采样全程11，RSS基线2,396,160B、峰值3,244,032B；撤载后高水位保持，不能据此声称内存返回初始RSS或无限期无泄漏。FD基线/各阶段结束15，采样峰270；最终各循环连接、排队、Task与HTTP缓冲零。完整调用user139.32s、system29.00s、墙钟1804.42s、最大单进程RSS121,696KiB；这些包括发生器、构建与所等待子进程，不是native独享CPU或同时进程树总RAM。四阶段实际合计1800.455秒。

完整artifact id `12393191627`（52,922B）下载SHA-256、ZIP CRC均核对；本机保留于 `Downloads/Ku_多循环验收_2026-10-21/formal-a31c113/`。提交、CLI/native/driver/fixture摘要、CPU环境、逐阶段分母、完整资源与成本见[精简原始摘要](evidence/formal-30m-linux.json)。这是一轮同机发生器、无代理/TLS/数据库的固定HTTP窗口，不解释历史七次504，不替代数据库、服务协调器、其他平台或跨主机验收。

## 此前窗口（保留历史身份）

首轮 [run 39685767469](https://github.com/fengyanweb/ku/actions/runs/39685767469) 使用 `eafe8382354570c26715146794ea767798d413f5`，因新发现的 Task 适配越界读主动取消，未完成正式窗口；中间证据和修复见[推进记录](multiloop-task-progress.md)。该首轮不计通过。macOS 短预检出现的两个 hot 504 不因下列 Linux 的结果而获得解释或修复结论。

修后 [run 39688747675](https://github.com/fengyanweb/ku/actions/runs/39688747675) 成功。被测 head 为 `d1738ef42ae82387815047c9f6d163917ae48f5d`，干净源码；CLI SHA-256 为 `4049d211bebfc004c8b860258a868090e56169a232880b3ee495d60da6563904`，native 为 `b1e52e94b15c1e4eab757d87fc278accf01e8f94784e7f4c2aa9d08a208698c6`。Ubuntu 24.04 / Linux 7.0.0-1017-azure、4 个可用逻辑 CPU（VM 暴露 2 核×2 SMT；不称四个独享物理核），affinity/可见 quota 检查有效容量 4，GCC 13.3、Python 3.12.3、Rust 1.89.0。仍为同机发生器、无代理/TLS/数据库的本固定 HTTP 窗口。

| 阶段 | 到达数 | 实际结果 | p95 / p99 |
| --- | ---: | --- | --- |
| warm 60s | 3,000 | 3,000 正确，错误/主动丢弃 0 | 24.8 / 24.9ms |
| steady 1470s | 73,500 | 73,500 正确，错误/主动丢弃 0 | 24.8 / 25.0ms |
| overload 30s | 4,500 | 1,460 正确；351 HTTP503；19 对端传输终止；2,670 压测端额度拒绝 | 912.1 / 941.9ms |
| recovery 240s | 12,000 | 12,000 正确，错误/主动丢弃 0 | 25.5 / 25.6ms |

共 93,000 次测量到达，89,960 次正确；普通三阶段合计 88,500 次全部正确。过载的 2,670 次发生器拒绝没有从分母删除；19 次直接对端关闭单列，不能写成 HTTP503 或逐个确定关闭分支。另有 8,073 次慢体连接：4,137 HTTP503、3,712 HTTP408、224 在阶段结束由发生器关闭；不与完整请求混用单位。撤载后 **258.181ms** 正确响应且队列/Task/缓冲归零，低于冻结的五秒门槛。过载前各循环累计延迟峰值 1ms；恢复后仍为累计峰值，不冒充独立恢复延迟。

native 线程始终采样为 11；RSS 基线 2,400,256B、采样峰值 3,252,224B，撤载后四分钟保持该值；FD 基线/各阶段结束均 15、采样峰值 271。最终各循环连接/活动/排队/Task 为零，HTTP buffer 为零、峰值 603,776B，正常 drain 退出 0。RSS 高水位保留约 0.81MiB，不代表逻辑责任人仍占用，也不能据一个窗口证明无限期无泄漏。

整个驱动及所等待子进程共 user 142.52s / system 75.36s，墙钟 1807.61s（含 native 构建/收尾），最大单进程 RSS 121,928KiB；后者可能是编译器/发生器，不是 native 常驻内存或同时进程树总和。正式负载按四阶段共 1800s 计划发流量，各阶段等候在途完成后的实际合计为 1800.529s，未把编译/排队算进去。

该历史窗口的完整 CI artifact id `12389980003` 保留计划、输入摘要、报告、退出/清理记录、环境与成本；本机副本在 `Downloads/Ku_多循环验收_2026-10-21/formal-d173-passed/`。此后 `9ac5ed9` 删除 driver 锁内多余的尾部自通知，`a31c113` 另修验收进程收尾并新增测试专用 Cargo example；因此旧窗口只保留其原提交结论。本页上方的新窗口已经独立完整执行，旧通过不替代它。两次窗口均不解释历史七次 504，也不认证跨主机可用性。
