# 多核执行与并发容量：执行边界及验收口径

核对日期：2026-10-22（用户时区）。本页记录 B01 的源码关系与候选机制，**不代表 B01—B08 已验收通过**。当前比较基线为 `c1de5e3245bff62eb0b422d5c39a5212a1d1723b`；未提交候选改动、最终提交、编译器、生成 C、native/夹具/驱动 SHA-256 须分别保留，后续由最终报告补齐，不能以分支名代替产物身份。

## 一页执行关系

| 路径 | 真实执行位置与源码入口 | 共享边界 |
|---|---|---|
| 接收、HTTP 解析、部分读写 | `c_http_runtime.c::ku_http_loop_run`；每条连接固定 owner，poll/WSAPoll 唤醒该线程 | listener 与轮转令牌在短 group 锁内；实例总连接、活动/排队额度和缓冲账本共享 |
| async handler | HTTP owner → `c_task_http.rs::ku_http_task_pump` → driver pump/run_one → control poll → adapter resume → `c_task.rs::_drive` | ready 链属于固定 owner，但队列发布、代次、终态和 Task 总账仍受一把 publication mutex 保护 |
| 同步 handler、数据库调用 | `c_http_runtime.c::ku_http_worker` 的有界 HTTP 后台队列 | 全实例固定后台份额、DB 连接/等待/字节额度；它们的吞吐不是事件线程扩核证据 |
| Task 普通 TCP、timer | TCP readiness 回到 owner；timer 是单调绝对期限及 driver 注册记录 | `c_task_reactor.c`、driver timer 表；DNS 解析仍走后台，不把全部网络流程称为事件线程操作 |
| Task 文件、目录、TLS | `c_task_host.rs` → `c_task_blocking.rs`，真实操作由共享有界 Task 后台 worker 执行 | 文件/TLS/DNS 会竞争同一后台份额；完成前保留原生作业、输入、输出与预算责任 |
| 通知、取消、终结 | 定向非阻塞通知；RUNNING 时 wake 只保留 notified；唯一 control executor 提交结果/清理 | registry/execution 真 lease 保持 frame/control 存活；物理释放及 disposed 之后才退款和复用代次 |

容量夹具由 `multiloop_load.Service` **直接启动 native**，没有为压测串入 `ku supervise`、`ku service`、代理或数据库。其管理入口是 native 主线程的独立 loopback 通道。`ku service` 的外部常驻恢复链属于原有独立验证，不能借用它证明本页的 CPU 容量。

## 锁范围与候选片段

`run_one` 在锁内取得唯一 RUNNING 责任及 execution lease，**解锁后**调用 control poll、用户 frame、drop；回锁后提交状态、队列与账本。HTTP OS poll 和内部 worker 的 I/O wait 也不持该锁。control executor CAS 约束单 Task 不重入。全局锁并未包住整个用户 CPU 循环，但每量子的状态交接、默认 intent 与拥有数据核算仍可能产生争用；timer 到期和管理指标仍有容量有界的整表扫描。

候选实现保持每次 frame 恢复的 **64 个 state dispatch**，仅在真正预算耗尽分支写当前 TLS 栈记录的 cooperative marker。driver 在同一 RUNNING/execution lease 内可继续 poll，至多 `min(调用者剩余量子, 32)`；每个 64-dispatch 边界检查单调时钟，达到 1ms 即返回原提交路径。**这不是 1ms 硬上限**：一个量子、容器操作或昂贵释放本身仍可能超出该值。

每 poll 清空本地 marker；仅 `Pending + cooperative marker + phase LIVE` 且未触达显式 intent 写时续接。timer/child/I/O 等 WAIT、显式 YIELD、finally/取消清理、无进度源 Pending 都不据枚举猜测续批。任意原锁内 intent 写都会标记当前 executor 并终止片段，即使随后又耗尽 dispatch，也不继承上一量子的共享意图。默认 generated YIELD 在匹配 driver/slot/generation/真实 control 的 TLS 内记录，回锁后仅补空 intent；raw adapter 无匹配上下文时沿用旧锁路径。外层 TLS 保存/恢复，lease 在整个片段保持存活。

HTTP pump 总额仍是 32，`pump(1)` 仍只消耗 1，driver/worker polls 按实际次数累计。没有改 ready 队列架构、charge 缓存、线程总额或超时；收益须由关闭诊断的同输入 release 对照证明，否则不应仅凭机制保留优化。统一 `emit_frames` 在生成 frame 前先输出 driver/helper 和 host 类型；普通 frame-only 入口不启用 cooperative 分支，不存在缺少 helper 声明的生成路径。

实际生成 C 的有界计数说明，仅合并交接仍不足：128,000 次迭代每请求约 8,008 个 control poll，N1/N4 的 reconcile 分别占 driver 锁调用约 63.69%/83.60%，HTTP readiness 检查约 31.76%/10.41%；合并后 run_one 交接只占约 2.10%/2.77%。一秒线程采样同时捕获 mutex 等待和 reconcile→map_charge 栈。计数覆盖诊断启动、采样及回收，不能直接当作正式计时窗口或逐请求精确占比；原始身份和精简线程记录见 `evidence/multicore-initial-diagnosis.json`。

据此增加保守的单量子证明：仅 Int/Bool/Null 的常量、读写、运算和无资源释放，以及已验证的空 scope 控制转移，才保持本次局部 `charge_unchanged` 标志。未知操作、owned 值操作、调用、等待等一律清除标志。只有真实预算耗尽的 Pending、匹配的 TLS/代次/control、phase LIVE、未触达显式 intent，且没有活跃 child、receipt、wait、drain、payload/result 时，adapter 才省略本次重复扫描。含 Blocking 或 finally 的函数静态禁用，以覆盖外部输出和异常隐式释放。没有缓存共享 charge，也没有延迟分配准入或最终退款；完整、失败、混合量子及 take/drop 等原核对仍执行。这是局部 Task IR 证明，不进入暂停的通用值或 AST 分析。

### 网络空扫描的独立争用

前述统计只覆盖 driver 锁，遗漏了另一把 blocking pool 锁。`c_task_reactor.c::ku_task_net_interests` 旧实现每次 HTTP turn 扫 1,024 个槽，对每槽取锁两次；CPU-only、NET_WAIT 为 0 时仍如此。真实生成 C 的一秒诊断中，N1 20,033 次 scan 对应 41,027,584 次 pool 锁；N4 每 owner 约 4,011—4,013 次 scan，每 scan 同样 2,048 锁。N1 scan 墙钟合计 0.477s，N4 各线程合计 3.217s，后者重叠且包含争锁，不能当作 CPU 秒。单独优化 driver 后 Linux 短对照仍有 N4 回退，未启动正式容量；这些失败继续保留。

最小修复是入口一次持锁扫元数据，非活动槽直接跳过。确需推进网络操作时先解锁，dispatch 完再加锁；重新读取当前 state、owner、socket 和 generation，形成一致的 poll snapshot。OS 操作仍在锁外，取消、期限、容量错误及迟到代次契约保留，无新队列或计数字段。永久回归检查空表仅一次锁、解锁期间代次/owner/DONE 变化、锁失败和容量错误都不遗留持锁，再复用真实 TCP/HTTP 取消及回收检查。

曾试验私有 HTTP handle 的负向 LIVE 快返；只移除该分支的相同生成 C 对照约 446.4 请求/秒，保留为 443.0，未证明收益。该 7 行分支及其专用新增测试已撤掉，HTTP readiness 继续完整走原锁。没有为了保留优化而扩大实现。

当前本机短诊断（非正式验收）在相同 128k 输入下，N4 的旧 driver / 全部候选分别约 128 / 455 请求/秒，均使用新 reactor。最终 N1 约 139；旧 5000-loop 路径最终 N4/N1 约 8,447/3,480。旧 reactor 的早期 64 请求/秒样本误用了 8 客户端，其他 CPU 变体为 32，不能作为独立 reactor 加速比；最终比较已统一为 32，空扫描锁计数证据仍有效。5s 开放过载 11,385 个计划全部启动，1,922 正确响应、9,463 明确 503、无传输/期限失败；公平性探针通过。该短窗含本机资源竞争及收尾，不替代三轮 30s 扩核、30s 等待/过载、三平台回归或最终持续窗口。

最终连接占用按每档 30s 冻结；64/128 客户端档至少在半数有效样本及最后 20% 窗口中分别观察到 58/116 条连接占用（轮换及管理请求按保守口径扣除）。256 档以至少 116 条及明确拒绝报告容量边界，不声称 256 同时持有。零创建、全拒绝、未完成连接或请求前缀、后段掉空都不能通过。Go CPU 余量使用 Go 自身包含收尾的时间作分母；事件线程仍用对应外层观察窗口。已用内存反例检查这些门槛，没有改服务超时或放宽原错误要求。

## 旧约 40ms 样本的证据边界

本地证据根目录为 `/Users/mac/Downloads/Ku_多核容量验收_2026-10-22/`：

- `linux-diagnosis-ci/multicore-capacity-ubuntu-24.04--9e3c87f6ba9ceb438f85506cb1da4fc8a084aeb4-1/{report.json,packets.txt,syscalls.txt}`：同一 keep-alive 的第 2—12 次请求约 41.9—42.1ms，空窗在请求提交至完整响应头之间，读完头后的正文仅约 10—13µs。系统调用显示状态行、各头片段与正文分次 send；包记录中第二次响应的 17 字节先于剩余 95 字节约 40.4ms，后者紧随 ACK 出现。这给出该样本的分段输出/ACK 等待证据，不证明历史 7 次 504 的原因。
- `local-diagnosis.json`：原 Mac 短采集未出现同样空窗，不能以“本机未复现”覆盖 Linux 记录。
- `mac-lock-diagnosis.json` 的锁计时是诊断构建证据；每次时钟采样会改变成本。`local-candidate-check.json` 的 TLS 独立候选无计时短对照仍受桌面 CPU 竞争影响，尚未证明收益。旧 196 请求/秒也不能当作服务容量。

当前输出候选 `ku_http_output_send` 使用 bounded sendmsg/WSASend 一起发布相邻头/体，保留部分写回、EAGAIN 和每轮字节/调用上限；实际改善及平台结果由后续无重诊断验收填写，不增加 timeout 或盲目打开 TCP 开关。

## 固定资源与支持边界

- N=1/2/4 只改变真实事件线程数。后台合计固定 **5**：HTTP 同步 handler 4、Task 文件/TLS/DNS 1；`KU_THREAD_BUDGET=16`，不能让 N=4 隐式多拿后台份额。Ku 显式线程数与 OS/FFI 辅助线程、栈预留、RSS/Windows 提交内存分别记录。
- 夹具全实例上限固定：连接 256、活动请求 128、pending 128、HTTP 缓冲 8MiB、Task 接入账本 16MiB；不是每 owner 一整份。固定 owner 的本地活动/排队份额仍可能先饱和，需记录真实接入/拒绝；不能只看全局总量推断所有循环均可接入。
- `KU_TASK_MAX_BYTES` 不是任意 Task 全堆计量，HTTP 账本不是整体 RSS；第三方库、分配器保留、线程栈另有归属。Linux cgroup v2 / Windows Job 的进程限制与这些账本不同；macOS 原生不提供对应隔离，外部 supervisor/代理也不自动处于限制域内。
- native async lowering 目前只接受已支持的 Task 原语和已知顶层 async helper。没有 PostgreSQL/MySQL/Redis Task 操作或通用同步调用桥；直接在 async handler 构造/调用此类 DB client 会在 native async 子集诊断处拒绝，不能解释成已异步执行。现有 DB handler 走同步 HTTP 后台；阻塞不能偷偷搬回事件线程，数据库不自动重试。
- 等待容量、CPU 扩核、热点公平、慢体连接、服务过载各自成账。35s timer 的专用 45s handler 期限仅用于等待夹具，不改变原正式持续夹具的 1s 期限；完整响应才计成功，发生器未发出/主动关闭、服务拒绝、传输失败和结果未知分别保留。

## 待补最终证据

保留当前必要定向回归记录；本页不预写扩核、Mac 回退修复或 B01—B08 通过。由最终采集补：源码/产物身份、N1/2/4 重复对照与有效 CPU、发生器余量、32/64/128 真实等待、热点与 timer p99、服务端过载/撤载恢复、资源趋势、最终必要 CI。官网、本版旧标签、JSON 旧默认及两个暂停模块维持原边界。
