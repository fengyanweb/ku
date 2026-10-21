# 多循环正式 30 分钟验收计划

状态：独立 CI 入口已实际启动；首轮因发现新的 Task 适配越界读主动取消，**尚未取得完整通过结果**。修后提交需按本计划重新执行。它不替代三平台功能 CI，不部署官网，不重跑历史 504 或 36 次内存验证。

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

## 结果填报

首轮 [run 39685767469](https://github.com/fengyanweb/ku/actions/runs/39685767469) 使用 `eafe8382354570c26715146794ea767798d413f5`，因新发现的 Task 适配越界读主动取消，未完成正式窗口；中间证据和修复见[推进记录](multiloop-task-progress.md)。修后完整结果待执行，须填写被测 head、run 链接、硬件/quota、实际请求分母、错误分类、延迟、撤载恢复、资源趋势及退出结果。macOS 短预检出现的两个 hot 504 不因 Linux 的结果而获得解释或修复结论。
