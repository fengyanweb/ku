# 多核执行与并发容量：执行边界及验收口径

核对日期：2026-10-22（用户时区）。已完成实现及任务书指定的平台主验收：Linux 完整 CPU/等待/公平/过载，macOS 最终扩核/回退与受影响路径，Windows 受影响回归及数据库/宿主补验。**macOS 额外 991rps 业务压力仍未收口，不能宣称全平台全部压力门槛通过。** 比较基线固定为 `c1de5e3245bff62eb0b422d5c39a5212a1d1723b`；各实际源码、CLI、生成 C、native、夹具与驱动 SHA-256 见 `evidence/multicore-acceptance.json`，所有原失败和取消结果保留。


本轮最终交付：R01—R04准备、C01—C06和新增T01—T08固定比较/差距处理已完成；当前候选8abb完整CI及三平台实际包验证通过。**未建立通用性能竞争优势**，按独立实验版本交付；PR尚未合并、新版本/标签/公开发布及A19真实环境未执行。下文保留每个旧失败和新增压力失败，完整执行不等于所有SLO通过。官网没有上传。

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

后续正式 Mac 过载给出不同负载下的新证据：128 活跃/128 pending 时，本机一次 5s 诊断记录约 1,573 万次 HTTP readiness，其中超过 99.996% 仍为 LIVE；四个 owner 的线程样本均包含 readiness→driver mutex 等待。诊断有计数/锁计时扰动，不用它计算收益。另一次无探针的同输入配对固定 374 次/秒、各 5s、原 3s 客户端期限：原路径最终 523 正确/1,181 明确 503/166 超时，候选 769/1,101/0；计时窗口内正确吞吐 87→102.8 请求/秒。最终计数包含不同收尾时间，不能把 769/523 当吞吐加速比。这是一次短配对，仍须正式复验。

据此重新采用有具体负载收益的小分支：已验证的私有 handle 仍独占 owner lease、尚未取消/finishing、ABI/尺寸/对齐有效且 atomic phase 为 LIVE 时，只返回 Pending；不消费值，不读取未加锁的 driver slot。其它 phase、取消和回收继续原锁路径。必要永久回归覆盖负向探测不推进、完成/取消/finishing、失效 lease 与 ABI、退款/释放恰好一次。前述 32 客户端无收益与本次 128 在途争用分别保留，不混写成相同负载的反转。

当前本机短诊断（非正式验收）在相同 128k 输入下，N4 的旧 driver / 全部候选分别约 128 / 455 请求/秒，均使用新 reactor。最终 N1 约 139；旧 5000-loop 路径最终 N4/N1 约 8,447/3,480。旧 reactor 的早期 64 请求/秒样本误用了 8 客户端，其他 CPU 变体为 32，不能作为独立 reactor 加速比；最终比较已统一为 32，空扫描锁计数证据仍有效。5s 开放过载 11,385 个计划全部启动，1,922 正确响应、9,463 明确 503、无传输/期限失败；公平性探针通过。该短窗含本机资源竞争及收尾，不替代三轮 30s 扩核、30s 等待/过载、三平台回归或最终持续窗口。

最终连接占用按每档 30s 冻结；64/128 客户端档至少在半数有效样本及最后 20% 窗口中分别观察到 58/116 条连接占用（轮换及管理请求按保守口径扣除）。256 档以至少 116 条及明确拒绝报告容量边界，不声称 256 同时持有。零创建、全拒绝、未完成连接或请求前缀、后段掉空都不能通过。Go CPU 余量使用 Go 自身包含收尾的时间作分母；事件线程仍用对应外层观察窗口。已用内存反例检查这些门槛，没有改服务超时或放宽原错误要求。

## macOS 即时探测的独立成本

最终共享路径修后，本机 N1/512k/32 客户端仍只有约 0.56 事件核，ready Task 持续为 31—32，发生器 CPU 约 0.004 核。一次有界采样把空窗定位到 `ku_http_poll(..., 0)`：192,047 次调用中每 64 次采一次的 3,000 次样本，墙钟合计 40.282ms、同线程 CPU 7.788ms；唯一事件线程的 93 个栈样本有 57 个位于该调用。稀疏估计和采样扰动已保留，不能据此唯一归因于内核等待或 OS 调度，也不是历史 504 的解释。

独立、无计时探针的 A/B/B/A 四个 5s 窗口固定 512k/N1/32：原路径 34/32 请求每秒、0.566/0.597 事件核；候选 64/64、0.960/0.961，全部正确、Task 回收和自然退出通过。候选仅在 macOS 零超时、已知 socket、非零 IN/OUT 且 fd 可放入 `fd_set` 时先做 `select(0)`；只有负向结果直接清 revents 返回。正向、错误、零/未知 interest、高 fd 和非零期限仍调用原 poll 产生事件。没有跳过 I/O turn、增加 dispatch/pump 预算或保留新内核注册。永久回归与正式结果另列；短测不预写通过。

该限制依据当前调用链与 [Apple socket readiness 实现](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/sys_socket.c) 审查；不把主分支源码当作所有已发布内核的完整证明，也不宣称适用于任意设备。全部原始文件身份和两轮数据见 `evidence/multicore-initial-diagnosis.json`。

## 旧约 40ms 样本的证据边界

本地证据根目录为 `/Users/mac/Downloads/Ku_多核容量验收_2026-10-22/`：

- `linux-diagnosis-ci/multicore-capacity-ubuntu-24.04--9e3c87f6ba9ceb438f85506cb1da4fc8a084aeb4-1/{report.json,packets.txt,syscalls.txt}`：同一 keep-alive 的第 2—12 次请求约 41.9—42.1ms，空窗在请求提交至完整响应头之间，读完头后的正文仅约 10—13µs。系统调用显示状态行、各头片段与正文分次 send；包记录中第二次响应的 17 字节先于剩余 95 字节约 40.4ms，后者紧随 ACK 出现。这给出该样本的分段输出/ACK 等待证据，不证明历史 7 次 504 的原因。
- `local-diagnosis.json`：原 Mac 短采集未出现同样空窗，不能以“本机未复现”覆盖 Linux 记录。
- `mac-lock-diagnosis.json` 的锁计时是诊断构建证据；每次时钟采样会改变成本。`local-candidate-check.json` 的 TLS 独立候选无计时短对照仍受桌面 CPU 竞争影响，尚未证明收益。旧 196 请求/秒也不能当作服务容量。

当前输出候选 `ku_http_output_send` 使用 bounded sendmsg/WSASend 一起发布相邻头/体，保留部分写回、EAGAIN 和每轮字节/调用上限；实际改善及平台结果由后续无重诊断验收填写，不增加 timeout 或盲目打开 TCP 开关。

## 固定资源与支持边界

- N=1/2/4 只改变真实事件线程数。后台合计固定 **5**：HTTP 同步 handler 4、Task 文件/TLS/DNS 1；`KU_THREAD_BUDGET=16`，不能让 N=4 隐式多拿后台份额。Ku 显式线程数与 OS/FFI 辅助线程、栈预留、RSS/Windows 提交内存分别记录。
- 上述 N 是验收时显式设置的 `KU_EVENT_LOOPS`，不是运行时默认值。未设置时，`c_runtime_config.c::ku_runtime_loop_count` 按可用 CPU、最多 8 循环及 `KU_THREAD_BUDGET` 剩余额度的一半自动选择；HTTP 启动还会按全实例活动、pending 和剩余 HTTP 后台份额缩小。显式循环数最大 32，超过实际容量或后台配额会拒绝启动，不静默增加额度。
- 夹具全实例上限固定：连接 256、活动请求 128、pending 128、HTTP 缓冲 8MiB、Task 接入账本 16MiB；不是每 owner 一整份。固定 owner 的本地活动/排队份额仍可能先饱和，需记录真实接入/拒绝；不能只看全局总量推断所有循环均可接入。
- `KU_TASK_MAX_BYTES` 不是任意 Task 全堆计量，HTTP 账本不是整体 RSS；第三方库、分配器保留、线程栈另有归属。Linux cgroup v2 / Windows Job 的进程限制与这些账本不同；macOS 原生不提供对应隔离，外部 supervisor/代理也不自动处于限制域内。
- native async lowering 目前只接受已支持的 Task 原语和已知顶层 async helper。没有 PostgreSQL/MySQL/Redis Task 操作或通用同步调用桥；直接在 async handler 构造/调用此类 DB client 会在 native async 子集诊断处拒绝，不能解释成已异步执行。现有 DB handler 走同步 HTTP 后台；阻塞不能偷偷搬回事件线程，数据库不自动重试。
- 等待容量、CPU 扩核、热点公平、慢体连接、服务过载各自成账。35s timer 的专用 45s handler 期限仅用于等待夹具，不改变原正式持续夹具的 1s 期限；完整响应才计成功，发生器未发出/主动关闭、服务拒绝、传输失败和结果未知分别保留。

## 最终源码与固定验收

生产修复冻结于 `245b9afe22a0eeb7b25a7d9a79e699cb75ae77a0`，容量驱动在 `461b46aa4474206f727afe06a9ac02be2a111bd8` 另行隔离等待组期限；两者间唯一差异为 `tests/acceptance/multicore_capacity.py`。专用 timer/TCP 等待组请求期限 45s，结束并退出该实例后，公平性、业务过载和连接组在新实例恢复原默认 30s。Go CPU/热负载/业务压力客户端统一绝对期限为3s；专用等待使用独立selector客户端（timer50s、TCP40s），详见下表。原持续夹具的 handler 1s、timer 25ms 和 50rps 普通负载未改动。

最终容量为 Linux/macOS 同输入 N1/2/4，预热 5s、计时 30s、各三轮交错，闭环 32 客户端；工作量先校准一次再固定。无代理、TLS 或数据库，事件线程执行真实 async 整数计算，运行时 query 输入决定工作量，8 字节 checksum 进入响应并逐次校验。旧 CPU5000 路径单列 8 客户端短对照，不能拿它替代重计算容量。driver 与 reactor 修前派生变体均使用最终 frontend、同一夹具、`clang -O2` 和相同客户端数；原源码固定为 `c1de5e3`，不冒充整个历史编译器构建。

专用等待为 timer 32/64/128、TCP 64，每档稳定观测 30s；热点混合验证普通、timer 和后台文件在热点仍运行时推进，p99 门槛 250ms。开放业务压力先测普通容量，再冻结 5 倍到达率 30s，最多 512 Go worker/256 排队描述符，撤载恢复门槛 5s。慢体连接 64/128/256 各 30s 单独成账，256 档是边界施压，不宣称 256 活跃 handler。所有未启动、明确 503、期限/传输失败及收尾单列。

对应检查为 [最终容量](https://github.com/fengyanweb/ku/actions/runs/39778970885)、[完整三平台](https://github.com/fengyanweb/ku/actions/runs/39778863985)、[对应 30 分钟](https://github.com/fengyanweb/ku/actions/runs/39778033004)。完整三平台原run仍为失败；Windows PG/host定向补验见下表，按未变组件组合证据。源码、产物、每轮结果、旧失败与取消记录见 `evidence/multicore-acceptance.json`。`39778024043` 因专用期限泄漏到后续测试组而取消；`39777976675` 由 pipeline 在后续驱动提交时取消，已完成部分不当作整轮成功。

## 正式数据（固定 512k 工作量）

| 平台 / 循环 | 正确吞吐中位数 rps | 事件线程核数中位数 | 三轮正确请求 p99 范围 | 每正确工作事件 CPU |
|---|---:|---:|---:|---:|
| Linux N1 | 68.267 | 1.000 | 480.6—487.1ms | 14.54—14.69ms |
| Linux N2 | 139.733 | 1.983 | 239.4—247.1ms | 14.17—14.18ms |
| Linux N4 | 160.333 | 3.889 | 210.5—212.2ms | 24.11—24.42ms |
| macOS N1 | 65.067 | 0.978 | 546.9—553.9ms | 14.87—14.93ms |
| macOS N2 | 133.333 | 1.905 | 271.8—283.6ms | 14.19—14.24ms |
| macOS N4 | 198.133 | 2.715 | 219.3—266.3ms | 13.68—13.74ms |

所有 18 轮结果均正确并与服务接收量对账，Go 压测端平均仅约 0.004—0.017 核。每轮 CPU 分母使用同一外层观察窗口，吞吐使用 Go 自己的 30s 计时，收尾正确结果不回填计时吞吐。Linux 可用 4 CPU；Mac runner 的 3 个可见 CPU 由四事件线程共享。N4/N1 中位吞吐比为 Linux 2.349、Mac 3.045；Linux N4 的每工作 CPU 成本上升，保留共享发布/全实例管理开销，不宣称线性扩展或所有负载均应选 4。CPU 轮 native RSS 峰值分别约 2.95MiB/4.22MiB，事件循环1/2/4对应进程线程8/9/11；这些是指定夹具采样，不是进程内存硬上限。

两平台 timer 32/64/128 均达到并在各 30s 窗口维持目标，空闲等待 CPU 约为单核的 0.066%—0.133%；128 档无额外普通请求额度承诺。TCP64 同时由服务和受控对端确认等待，恢复67.45/68.74ms。热点期间三条 probe 每完整秒持续完成，Linux ok/timer/file p99为1.77/26.46/2.29ms，Mac4.54/53.18/20.20ms，均低于250ms。连接64/128/256的独立30s占用边界检查通过，结束后队列、Task、连接与字节归零。

Linux 开放业务压力冻结为802rps，30s计划和实际均24,060：4,163正确、19,897明确503、其它/传输/未完成/未启动均零。独立Python probes另计104次：31正确、69明确503、1次RemoteDisconnected、3次1s客户端超时；不声称全部流量零传输失败。服务计数另含独立probe，保留下界及差额。撤压恢复上界1.311784s。Mac 原CI预检99.0667rps在2s窗口计划199、实际198且全正确，最后计划仅距截止1.346ms，小于该样本调度延迟p99=1.9ms，所以严格门槛失败、没有启动过载；原run保留失败。预检现在先向上量化为100rps，严格未启动零门槛不变；本机补验200/200正确，30s的991rps全部29,730次发起，7,302正确/22,427明确503/1次EOF，独立probe另有1次reset；该旧补验仍为not_closed，逐连接关联另外记录。

对应生产源码的 Linux 30 分钟 [39778033004](https://github.com/fengyanweb/ku/actions/runs/39778033004) 已通过：普通阶段88,500次全部正确、未启动/客户端拒绝均零，warm/steady/recovery p99为25.5/25.5/25.6ms。过载计划4,500、实际1,812，其中1,467正确、326明确503、19次直接连接终止；2,688发生器额度拒绝单列，不能据此宣称实际150rps。撤载125.415ms恢复，最终记录/字节归零，正常退出0；RSS从warm约2.59MB到steady约2.61MB，压力后约3.26MB并在recovery维持，线程11、阶段末FD15。该窗口证明此配置持续结果，不证明历史七次504已修复，也不代替独立开放业务压力。

## 本机复现入口

需要 Rust 1.89.0、系统 C 编译器、Go 1.27.2、Python 3；依赖版本和 flags 由 CI 归档。使用干净的对应提交，保持资源预算一致；压测端同机时另报其 CPU 和采样成本。

```sh
cargo +1.89.0 build --locked --release --bin ku
python3 -u -B tests/acceptance/multicore_capacity.py --ku target/release/ku --mode capacity --output /tmp/ku-capacity-report.json
python3 -u -B tests/acceptance/multiloop_load.py --ku target/release/ku --loops 4 --duration 1800 --rps 50 --profile release --output /tmp/ku-sustained-report.json
```

命令中的临时 native/C/Go/夹具由现有 TemporaryDirectory 回收；只保留有界报告和失败证据。需要旧 Linux 包/系统调用证据时才使用 `--mode diagnose`，不在正式计时构建开启重诊断。

## 使用建议与未验证范围

需要保守起点时，建议显式设置 `KU_EVENT_LOOPS=1`；这是一项使用建议，代码默认按 CPU、线程预算和 HTTP 容量自动选择，并非固定 1 个循环。计算负载可在本次固定预算内选择 2 或 4，先按实际硬件和 workload 量测；I/O 等待容量与事件线程数不同，不为等待自动增加后台总线程。这里没有证明 32 循环、万级同时活动请求、任意长同步指令可抢占或所有数据库操作可进入 native async handler。高于 `FD_SETSIZE` 的 macOS socket 仍正确回落 poll，但其性能没有由负向探测短测证明。

运行时分配账本、RSS 与 OS 进程域限制分别解释；协调器/代理不自动受 worker 硬限制，单 Task 全堆未精确计量，macOS 原生对应硬隔离不支持。原 A19 跨主机演练仍缺授权环境，与本轮单机 B01—B08 独立。历史七次 504 仍未定因，不能用本轮新路径结果倒推其根因。数据库无 schema/migration 变化，也不自动重试或重放。官网、v0.0.18 标签、JSON 旧默认及两个暂停模块维持原边界。

## 交付清单与 CI 对应

| 编号 | 实际交付与验证范围 | 结论 |
|---|---|---|
| B01 | 执行关系、锁范围、owner/同步/后台边界；量子交接、资源核算证明、空网络扫描与LIVE探测热点修复 | 完成 |
| B02 | 有界Go发生器、独立到达、互斥结果账本、服务接收差额、CPU分母及采样成本；普通预检先向上量化整rps，未启动零门槛不变 | 完成 |
| B03 | 两平台最终N1/2/4各三轮，输入/预算/flags不变；CPU与有效工作一致，有可重复收益 | 完成 |
| B04 | timer32/64/128、TCP64、固定线程、热点期间逐秒推进、30s连接占用及资源回落 | 完成 |
| B05 | Linux完整开放业务压力与连接压力通过，撤载≤5s；Mac附加压力有EOF与诊断超时未唯一归因 | Linux主验收通过；Mac附加项未收口 |
| B06 | Mac最终重计算及原CPU5000可比对照；旧N4回退未再出现，N1即时poll成本有配对证据与正式结果 | 完成 |
| B07 | 受影响永久回归、三平台默认suite、检查工具、真实PG/host；原失败+定向修正按源码映射组合 | 完成对应模块验证；不改原整轮失败 |
| B08 | 本报告、紧凑全轮原始数据、可复现入口、配置建议、平台限制与清理收据 | 完成交付，保留Mac附加未收口项 |

| 模块 | 实际源码/检查 | 结果与复用边界 |
|---|---|---|
| native runtime / 生成C | 生产245b9af；[Native461](https://github.com/fengyanweb/ku/actions/runs/39778863985)，实际merge树与head树相同 | Linux2014/Mac2015/Windows2008默认测试零失败；ASan/UBSan184、TSan选定13通过；Mac新增真实socket回归与三平台53/54/50受影响回归通过 |
| Windows真实PG/宿主 | [定向401d50c](https://github.com/fengyanweb/ku/actions/runs/39785156409)，仅验收driver变更 | PG与实际LocalService注册/恢复/停止通过；两次独立新读31+31ms恢复；DBpending/bytes0、退出连接0、进程与清理错误空 |
| 服务协调器/代理配置 | 本轮无生产改动；[c853完整CI](https://github.com/fengyanweb/ku/actions/runs/39772167091)已有相同有效Windows代码的Caddy验证 | Caddy替代375ms，更新17.125s/回滚16.156s；新定向run不包含Caddy。15s是单条命令期限，不能把整段写成小于15s |
| CPU/等待/公平/压力driver | 容量461，修正预检整rps与PG故障探测的driver401；诊断日志前缀另做脱敏 | 401未改变运行时或原持续夹具；旧Mac199计划/198发起的失败不改写；Go不自动重放失败请求 |
| 最终持续窗口 | [245源码30m](https://github.com/fengyanweb/ku/actions/runs/39778033004) | 与461 Linux容量CLI SHA完全相同；88500普通正确，零错误/舍弃，撤载125.415ms，记录归零、退出0 |

Windows 原整轮的具体失败是故障后第2个新请求 HTTP200/`error:acquire_timeout`，原日志只显示JSONDecodeError。源码中100ms acquire期限同时约束新连接建立，并未保证第二次请求必成功；这没有证明生产退化。修正验收为既有5秒恢复窗口内最多8个独立只读新请求，只允许已定义的query_error/execution_unknown/acquire_timeout作为中间状态，未知HTTP/传输/JSON仍立即失败，完整expected必须返回。原始故障/错误计算入记录，写入不重放。该次定向实际只发两次，62ms恢复；旧初始socket期限会随分段读取续约，本轮修正验收驱动绝对截止后另做定向检查（C03）。

## Mac 附加压力的剩余边界

本机未插桩30s补验保留为`not_closed`：业务1次EOF尚未逐条关联。随后单次有界关联诊断只证实一个独立probe reset：同TCP连接的前次200完成，服务按既有压力策略在HEADER/无部分数据/无Task状态关闭，客户端46.545ms后才复用；这不能自动解释原业务EOF。诊断构建另出现138次完整响应头等待超时，保留4个样本均为新连接，不能扩大成138个已逐条确认，也不能冒称HTTP504或全部归为idle回收。该构建修改了临时结构和时钟/连接元数据，具有测量扰动，不用它推导未插桩性能比。

上一阶段最后一次为固定6s的连接生命周期短采集：5946次全部启动，1693正确/4253明确503、业务异常0，另有1次独立probe连接错误；未取得≥2.5s异常生命周期，未触发线程采样。它不能代替30s门槛或覆盖旧失败。`accept→header_complete→async_submit→response_prepare→write/close`中的submit只表示提交，不伪称首次执行；内部原因未定位。该阶段采集已结束；本轮有界复验见C04，未放宽gates、未增加timeout、未重试请求，也未据猜测修改生产代码。

因此当前只交付已验证的Linux完整压力范围与Mac计算/等待/公平范围；**不承诺Mac在991rps该超载条件下无传输/期限失败**。后续若需要承诺该平台此类超载行为，应先取得未插桩异常的请求阶段和线程证据，再决定最小修复，不重跑无关长测或重做调度架构。

## 修改范围与清理

生产变更集中于`c_http_runtime.c`（bounded gather写回、Mac零期限负向探测）、`c_task.rs/c_task_adapter.rs/c_task_driver.rs`（64-dispatch量子证明与有界交接）、`c_task_reactor.c`（空网络扫描锁）及`c_task_http.rs`（私有LIVE负向readiness）；保留现有所有权、取消与最终回收协议。对应永久回归及原scheduler probe计数适配，测试SDK宏冲突/CRLF夹具修正不影响业务API。验收driver/夹具与本报告是任务书要求的永久复现材料，没有新增生产依赖。

数据库没有新增/修改/删除或migration；网站、v0.0.18标签、JSON与两个暂停项未动。原工作区87项修改的状态及tracked diff SHA与开始时相等；自有临时探针、生成C/Go/native、副本、测试目录与进程均清理，必要有界失败证据保留于报告根目录。实现工作树保留供PR审阅，不是垃圾副本；主工作区不自动覆盖或reset。最终只更新代码分支与PR说明，未合并、发布或部署官网。

## C01—C06 最终复核（2026-10-22）

该轮确认并修复一处验收驱动总截止，补强已有HTTP永久回归，没有新增生产运行时改动。`git diff 245b9afe 45b3b29 -- src`为空：461b46aa只改容量期限隔离；401d50c只改预检和PG驱动；bcaf0d6只加报告/证据及诊断正文脱敏；45b3b29只改PG驱动/单测及HTTP已有回归。后续752仅在`src/registry_server.rs`增加/调整`#[cfg(test)]`内容，不改变该生产等价结论。数据库schema、JSON默认、两个暂停模块及网站未动。

### C01 产物与最终旧路径

最终容量目录名中的bb9ca12是workflow merge SHA，report实际受测源码为干净461b46aa，两树相同。Rust/cargo1.89.0、Go1.27.2；Linux Ubuntu clang18.1.3 x86_64，Mac Apple clang17.0.0/Xcode16.4 arm64。CLI用`cargo build --locked --release --bin ku`，native用`ku build --backend c --profile release --emit-c`、C11/`-O2 -pthread`。两平台夹具相同，peer端口展开导致expanded-source不同。所有完整身份见原JSON的frozen_capacity_runs及新增final_legacy_cpu5000_summary。

| 最终产物SHA-256 | Linux | macOS |
|---|---|---|
| CLI | 05be28e442c2fed0475ff89111f4052dd518f80de83bdb98e3f7d839f8f9f930 | 0d7bf0bbccd6e64fc8f11e60abf3bbdd5dbd16cf0f92c35bf4e539b969d76cc6 |
| native | f756a4157e37fd12468034436d3cb643960679df020051e0d901d625a8f85209 | 7313c9bb852d014e99cc602984263ffd7451f38fd9d46aef57c394e668482538 |
| generated C | 93e1056c5fda877cf9c60be80ab4daf7ff381d3d0d4674ee6f4754141f9b1517 | 59c65537ebe42a84a335785be38c6b404cd6d22cd280b5a83ffc23edca0e48e4 |

最终CPU5000配对来自同一最终run的short_comparison：8客户端、1s预热+5s测量、三轮交错；各格按轮0/1/2列“rps / p99毫秒”，不是早期32客户端诊断。

| 平台/N | 最终frontend＋基线HTTP/driver片段 | 最终native |
|---|---|---|
| Linux1 | 191/43.1；195.2/42.1；191.4/43.1 | 5629.4/1.8；5546/1.8；5643.6/1.8 |
| Linux2 | 195.2/42.9；195.2/42.1；192.6/43.1 | 7881.2/1.3；8228/1.3；7593/1.7 |
| Linux4 | 195.2/43；195.2/42.4；194.6/42.4 | 8464.8/1.7；8523.4/1.7；8559.6/1.7 |
| Mac1 | 2876.4/3.7；2560/6.2；2986.8/3.6 | 6758.8/1.6；6788.6/1.9；6637/2.1 |
| Mac2 | 4146/2.6；4299/3.2；4345.8/2.8 | 11032.4/1.2；10801.6/1.4；10614.8/1.4 |
| Mac4 | 2928/7.6；2962/6.9；3004/8 | 12124.2/2.3；12011.6/2.3；12511/2 |

修前派生体保留最终reactor，只替换c1de5e3的HTTP/driver，恢复7处adapter入口、移除7处budget marker、禁用5处scalar快返，不代表整个旧编译器。原JSON保留36条配对；另6条capacity.legacy为正式CPU后的final-only5s/8客户端：Linux N1/2/4为5401.4/7905.8/8473.2rps、p99为1.9/1.3/1.7ms；Mac6765.4/10937.4/12493.8、p99为1.8/1.3/1.9ms。18轮512k原正式结果全部保留。

### C02 真实期限与无重放

| 组别 | 客户端期限 | 服务期限与成功定义 |
|---|---|---|
| CPU/legacy | Go raw TCP，实际worker执行起dial/write/header/body共用绝对3s | whole30s/handler45s；完整200且checksum/body相等 |
| timer35s | capacity_wait._Cohort selector；每socket创建起绝对50s；驱动总收尾began+51s | 专用whole45s/handler45s；真实35s后完整200/wait-ready，非取消 |
| TCP64 | 同一selector绝对40s；Ku net连接1s/读取40s | whole45s/handler45s；服务与peer持有64达30s，释放K后64个完整network-ready，释放起5s恢复 |
| 公平/业务压力 | Go同一绝对3s；热点4worker，开放压力512worker/queue256 | 新实例whole恢复30s、handler45s；终态互斥计数 |
| 公平/压力独立probe | Python runtime_matrix.call，1s socket操作期限，非绝对总期限 | 逐次分类；公平每秒推进且p99≤250ms；主动过载错误另计 |
| 等待普通probe | 同一Python路径，2s socket操作期限 | 等待实例45s；32/64timer及TCP完整正确，128timer不承诺额外普通额度 |
| 原30分钟 | multiloop_load.request，3s socket操作期限，非绝对总期限 | whole默认30s/handler1s/timer25ms；50rps普通窗口零错误，过载另列 |

源码入口：capacity_wait.py::_Cohort/timer_wait/network_wait、multicore_capacity.py::capacity关闭等待实例后新建普通实例、capacity_client.go::worker.request、runtime_matrix.py::call。最终timer实际完整返回约35.002—35.010s，线程均11，FD15→47/79/143→15；TCP实际观测30.001/30.002s，FD15→143→15，64/64完整返回、账本归零。原证据足够，不补跑等待。

Go调用链为ledger.start→w.request一次→必要时DialContext→SetDeadline(起点+3s)→Request.Write一次→自定义完整头读取/http.ReadResponse→有界完整体→终态分类。未使用http.Client/Transport，错误立即返回关闭；下一计划新建连接不等于重发失败计划，started对应一次请求尝试。保留keep-alive，不增加重试。

开放计划ceil(duration*rate)，目标began+index/rate，截止began+duration；scheduler过期、queue满、worker取到过期计划分别计scheduler_at_cutoff/client_queue_full/queued_or_closed_at_cutoff，不发送。已started只按原3s有限收尾，窗口外完成不回填吞吐。预检99.0667→100提高速率、最后目标由1.998654s提前至1.99s；旧199/198仍失败，未启动零门槛不改。

### C03 Windows绝对恢复截止

已确认缺口属于验收驱动：初始socket timeout不能约束BufferedReader多次读，慢滴头/体可续约。仅恢复分支使用DeadlineHTTPConnection/DeadlineSocket，每次connect/send/recv_into重算min(单次进入+4s,统一5s截止)-当前时刻，过期不建socket，完整读取/解码后再次检查。默认及持久路径不改，不增加业务timeout。5s从pg_terminate_backend返回且终止结果记录后起算，不包括终止SQL自身；最多8次独立新只读/read，每次新sequence/连接。未知HTTP/传输/正文/JSON立即失败；三种已定义中间错误不计成功；原丢失确认写、实际1行及无重放断言未改。

旧401d50c的31+31ms/62ms、退出前实际1条DB连接/退出后0是原相对样本，不能伪造新绝对时钟。新驱动45b3b29的[Windows PG/host定向run](https://github.com/fengyanweb/ku/actions/runs/39799865611)已通过，实际checkout45b3b29。终止1条DB连接后两次独立新读：sequence9在933.890→933.906s返回execution_unknown（16ms），sequence10在933.906→933.937s返回完整72B expected（31ms）；统一截止938.890s，累计47ms，没有续约成新的5s。DB pending/bytes为0，退出前实际1条连接、正常退出0后DB连接0；原丢失确认写实际1行且未重放。Windows原生宿主以LOCAL SERVICE注册三项，故障504为1000ms，2328ms首次不ready、2984ms替代gen2 ready，存活实例10/10正确，替代业务200，退出75原因如实保留、宿主停止0、残留进程与cleanup为空。此scope跳过Caddy，不能称本轮重验了Caddy。CLI SHA为1a52d1bdd6d7ef5451435e38a3191b29e95c6bc5c32cdf51e953770eb3c6afa1，宿主native SHA为62dff94fa24a72463cea8e58e86acbf96813710c4f4f98f11713c4bdc3f48384；PG使用原debug C夹具，未冒称release容量产物。新增正常响应、慢滴头/体、过期零socket、拒绝DNS五项永久回归复用test_http_pg_lifecycle.py，本机16项中15通过/1Windows ACL平台跳过；CI实际验收单测24/24、部署契约21/21、release CLI构建和Windows全workspace/all-targets clippy均通过。真实慢滴反例用130ms预算、35ms间隔：旧socket读取466ms后才完整返回，新绝对期限131ms即抛timeout；旧最终检查原本也会拒绝晚成功，本轮修的是调用及时终止。

### C04 Mac有界收尾

生产idle-pressure仅处理HEADER+reused+buffered=carry=0且先MSG_PEEK。内核已有字节保留；已读前缀、BODY/JOB/ASYNC/WRITE/REUSE_WAIT不进入。输出全部完成才next_request，未退休job/Task保持CLOSED而不立即复用。现有真实TCP回归有完整pipeline、pending满的明确503屏障、当前body完整响应后关闭及pending全部响应。新增控制回归提取实际pressure分支，在部分写/EAGAIN之间检查，核对游标/原deadline；覆盖queued字节、EINTR、空闲EAGAIN/EOF及8种非idle状态。真实TCP另验压力关闭后再复用只能FIN/RST而非新响应/超时。两个精确测试本机通过；Task代次/退休/退款联合引用已有driver、真实HTTP及最终资源回落，不称pressure harness单独证明全量计费。

只新增一次未插桩release30s991rps：normal200/200正确；业务29730全部started=6775正确+22955明确503，其它/传输/unfinished/notstarted均0。独立probe135次=46正确+88明确503+1reset，未逐连接关联；不能并成全流量零错误。撤载恢复上界1.351473s，20项资源归零，exit0。原fixture/flags/预算、Go绝对3s、whole30s/handler45s不变；完整身份和原结果在mac-close-final-release.json，本机M1并非独占环境。一次准备参数错误发生于服务/流量启动前，随证据保留，临时目录清理，不计额外压力窗口。

本轮未证明新的生产缺陷，未做猜测性修复。旧三项仍分开：release1次业务EOF未定因；原probe3 reset有唯一连接/时间关联，符合idle契约；插桩138次头等待超时仅4个明确fresh样本，未定因，非HTTP504。新release业务通过不覆盖旧失败，新独立probe reset也不能被旧probe3证据解释。按有界停止条件，Mac991附加项仍未收口，本轮停止采集，不再加第二个30s或新插桩碰运气。

关闭契约参考[RFC9112 §9.5/9.6](https://www.rfc-editor.org/rfc/rfc9112.html#section-9.5)，标准允许idle竞争不代表Ku任意关闭都正确，也不改变旧零传输失败实验的失败状态。

### C05 快路径与永久回归

| 变更 | 已有入口及边界 |
|---|---|
| 64-dispatch/TLS | native_task_source_test的quantum_default、segments_keep：嵌套TLS、timer/未知Pending、pump1/32、显式yield及1ms边界；HTTP pump仍32 |
| scalar charge | 同target的scalar_quantum：owned、child、Blocking/finally、溢出错误、cancel、最终reserved_bytes=0；未知/call/分配释放保守回退，未跳准入/最终退款 |
| reactor重查 | 同target的tcp_late_readiness及真实TCP：generation/socket/state/owner变化、dispatch/重锁失败不遗留锁 |
| 私有LIVE | native_http_live_task_readiness_preserves_completion_cancel_and_owned_lease：完成/cancel/finishing、无效lease/ABI/size/alignment、一次take/drop/adopt/refund；旧gen由driver退休复用旧wake=STALE及真实HTTP退休联合覆盖，不称LIVE单测直接注入旧gen |
| Mac select(0) | native_http_macos_zero_poll_preserves_real_socket_events_and_fallbacks：真实listener/TCP/UDP、关闭/半关闭/refused/满发送、fd/未知mask、errno/revents及原poll回退 |
| gather/close | native_http_gather_write_preserves_partial_vectors_errors_and_turn_budgets及native_http_reuses_framed_requests_and_yields_idle_connections：9vector/64KiB、262144B/32call、partial/EINTR/EAGAIN/peer错误/deadline、pressure状态与完整wire |

最终capacity affected日志逐条保留Linux53/Mac54/Windows50通过，Mac专有仅Mac执行。原三平台套件/ASan184/TSan13/真实HTTP及宿主结果按源码复用；本轮只补改动相关路径，不重复36次内存矩阵或30分钟。

### C06 四种状态和门禁

主验收：原任务核心功能与指定平台范围完成。Mac991附加项：旧异常未定因/未收口，不称全平台压力通过。A19：真实跨主机未执行，最少需授权两台独立宿主、可切换入口/健康路由、隔离PG主备故障域和允许故障注入的演练环境。PR2：仅提交/推送/更新，未合并、未打标签、未发布、未部署官网。

API实查main protection返回Branch not protected、repository rulesets为空，没有required checks；bcaf0d6的check-runs=0/status contexts=0，空集合state=pending不代表已有失败或绿色。原39778863985仍失败，39785156409定向通过；本轮实际驱动只补39799865611并已通过，不增加无关全量重跑。误指向旧bcaf的dispatch39799100744已取消，不作为修复验证。新CI实际checkout、模块结果和清理收据写入同一证据JSON。

复核与清理收据：同一原始根目录下completion-local-checks.json、completion-closeout-ci/{ci-receipt.json,windows-job.log,service-lifecycle-windows-2025/*.json}及mac-close-final-release.json；下载ZIP在内存校验CRC后只保留必要JSON/日志。主工作区87项修改的status及diff摘要未变；临时构建和socket/进程均清理，外置共享cargo缓存保留复用。子智能体任务结束后停止；本轮新增内容均为必要永久回归或紧凑证据，不保留一次性脚本。

## 本轮独立候选交付准备（R01/R03/A19，2026-10-22）

### 版本范围与发布状态

原定 v0.0.18 的发布归属以 [冻结清单](v0.0.18.md#冻结范围与原任务清单)、[R7.135 原决定](v0.0.18-worklog.md#r7135-v0018-release-scope-frozen-by-user-2026-10-20)及[修订版说明](v0.0.18-r1.md)为准。旧版已实际交付：[v0.0.18-r1 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) 绑定 `f0bb31f3420cde8e95ba3569f0fbd1af933c27bf`，[tag CI 39613361721](https://github.com/fengyanweb/ku/actions/runs/39613361721) 的 17 项 job 全部成功，包含三个目标的实际安装包、完整资产核验和公开发布；不能把旧日志中“待最终 CI/发布”的历史状态当成今天仍欠交付。main 上的对应合并为 `2cc964d8a6652486c71271f4fd3562ceafa59551`。

10 月 21 日原多事件循环任务书与 B01—B08 任务书已约定独立分支承载、不倒灌旧版发布范围。本分支是后续运行时候选，**未公开发布**，不覆盖旧标签、旧资产或旧版说明。此次交付核对起点为 `8210e5200c466d9041807d04a68c1844ba9cbd6f`、[PR #2](https://github.com/fengyanweb/ku/pull/2)（`codex/multiloop-runtime` → `main`）；生产修复及各模块有效证据仍按上文 SHA 对应。候选包内 Cargo/CLI/VSIX 暂保留 `0.0.18`，此号码不能区分旧发布包与候选，必须同时核对完整提交及 manifest、`RELEASE.json` 的身份；本节不决定新版本或标签。

本次代码、验收与打包候选为 `8abb9bc83cfe6b5c82fd16aa92a0f6d08c7dff39`。正式对照冻结的基准源码为 `752256d452fe5591290ea294db86ca4c3159c65f`，测量二进制来自生产等价的 `8210e5200c466d9041807d04a68c1844ba9cbd6f`。821→8abb 的唯一 src 差异在 Registry 的 `cfg(test)`；锁文件、构建配置及全部生产代码相同，后续改变仅涉及测试、基准、产物验证和文档。两个 T07 优化候选均未保留。文档收口提交不改变上述测量及安装包身份。

| 原要求/本轮工作 | 当前状态 | 具体剩余动作 |
|---|---|---|
| v0.0.18 冻结功能、0.0.18-r1 具体修复及安装发布 | 已公开交付；不重新增加发布前置 | 保留原下载、许可证和已知限制 |
| 多事件循环核心、B01—B08 指定平台主验收、C01—C06 | 按本报告及集中 JSON 收口；旧整轮失败、取消与定向补验分别保留 | 不重复无关容量、30 分钟或 36 次内存验证 |
| 新增 T01—T08 四栈业务、资源与竞争力对照 | 已完成固定比较、资源/恢复核对和两个主要热点的有界处理；未建立通用性能竞争优势 | 保留失败与采样边界，按实验定位交付；具体后续工作见 T07/T08 |
| 当前独立候选的安装产物与公开交付 | 8abb 三平台实际包、安装后多循环程序和完整必要 CI 均通过；PR 未合并，无新标签或 Release | 可以进入独立实验版本发布；最终合并、新版本号/标签及公开发布交由用户决定，不复写旧资产、不部署官网 |
| Mac991 附加异常 | 有界调查已结束，相关现象仍按 C04 分别保留 | 仅在出现新可定位证据、明确契约违例或承诺该场景时再开；不追加碰运气长测 |
| A19 真实跨主机演练 | 准备材料已有；缺外部环境，未实测 | 取得下文唯一环境信息组，再按原契约执行 |

完整 M:N、解释器全面挂起、剩余阻塞库与完整数据库 SDK/平台矩阵、延后的 Redis/MySQL 公开配置、单 Task 精确全堆计量及协调器自身 OS 限制仍按原[后续计划](roadmap-decisions.md#v0018-发布冻结后的计划)处理。当前分支实现的局部网络与后台路径不能据此把这些大项整体改成完成，也不把暂停模块或 Mac 附加未知临时变成旧版本阻塞。

### 随本候选交付的能力与边界说明

- **配置与执行路径。** native C 支持固定 owner 的多事件循环；默认由代码自动选择，保守起点可显式 `KU_EVENT_LOOPS=1`。`KU_BLOCKING_THREADS` 为全实例后台总数，`KU_THREAD_BUDGET` 默认 16；混合 Task 文件/网络适配会保留一个共享 Task 后台份额。`KU_HTTP_REQUEST_TIMEOUT_MS` 默认 30000，读取、排队、执行与写出共用绝对期限；超时不表示原生工作已物理停止。完整配置见[运行时配置](multiloop-runtime.md#配置与总量)。改变循环数须同时核对全实例活动、pending 与后台份额，不将每个循环预算重复相加。
- **异步能力与数据库。** native async 的已接入 timer、普通 TCP readiness 和已支持的 helper 在 owner 上推进；DNS、文件和 TLS 等既有适配仍使用有界后台。数据库 handler 通过同步 HTTP 后台持独占连接 lease，当前 native async 不支持直接调用 PostgreSQL/MySQL/Redis client。未接入组合明确拒绝；不新增隐式 clone、Result 自动解包或用户 spawn API。数据库操作和失败 HTTP 不自动重放，结果未知要保留未知，后续独立新请求成功不能证明原操作成功或回滚。完整边界见[数据库执行说明](database-runtime.md)。
- **预算与平台。** HTTP/Task 分配账本不是整体 RSS，单 Task 全堆未精确计量；线程栈、第三方库及分配器保留不能从该账本推导为零。Linux 的可选硬模式要求专用可写空 cgroup v2 委托、可用 memory/pids 控制器及祖先允许迁移；Windows 约束 Job 提交内存，提交失败不一定杀进程；macOS 原生明确拒绝对应硬隔离，普通服务恢复仍可用。受控域覆盖 launcher、native 服务及其第三方分配，supervisor/代理在域外；部署总预留不等于整个系统绝对封顶。`ku supervise` 管有限程序批次，`ku service` 管常驻实例，完整条件见[进程执行说明](process-execution.md)和[服务部署](../deploy/service/README.md#可选-os-限制)。
- **已验证范围与性能。** 本报告的 Linux/Mac 扩核、timer32/64/128、TCP64、公平性、Linux30分钟及 Windows 定向恢复只适用于各自列明的提交、完整软件栈、硬件和夹具；Linux N4 的单位工作 CPU 成本高于 N2。802/991rps 是过载输入，不是正确处理能力；CPU5000 配对是派生片段对照，不是整个历史编译器的通用倍数。T01—T08 已完成，未建立通用 HTTP 吞吐或 CPU 成本优势；下文区分同速率成本、有限合格容量及等待内存优势，不据架构、所有权或无 GC 宣称领先。
- **已知限制与兼容性。** Mac 旧 release 业务 EOF、已关联的旧 idle probe reset、插桩头等待超时与新未关联 probe reset 分开保留，不写成全平台所有流量零错误；A19 未实测。本分支结果不追认历史七次504根因。JSON 沿用旧默认；通用值复制/释放与 AST 后续分析继续暂停。0.0.x 不承诺内部 Task ABI/生成 C 兼容，候选部署应使用对应 CLI 重新构建并保留旧产物/配置供回滚；官网未上传或部署。

### A19 已有材料与一次性环境缺项

准备内容集中在 [deploy/service/README.md 的“多主机边界”](../deploy/service/README.md#多主机边界)（当前第155—221行），不另建一套 runbook。这里已有双宿主拓扑、配置/启动与停止命令、真实 readiness、单故障注入与撤销、恢复接流和回滚、请求账本、数据库确认/未知写入及 RPO 核对、最终资源清理；配套文件是 [cluster.example.json](../deploy/service/cluster.example.json)、[外层 HAProxy 配置](../deploy/service/haproxy.multihost.example.cfg)及同目录 systemd/launchd/Windows 宿主模板。独立客户端固定地址，发起窗口360秒、最多720次；1秒是单次socket操作超时，在途请求可能越过发起窗口。故障前20条正确、恢复以连续10条正确确认；动作前后使用同一客户端单调时钟，保留故障时刻区间。现有演练目标为宿主/入口10秒、数据库30秒，总恢复清理窗口120秒；设施不满足时须在故障前记录目标与依据，不能事后放宽。

**仍需提供唯一一组真实环境信息与权限：**应用宿主 A/B 的标识、OS/架构/资源/故障域和允许停启的范围；独立客户端及固定入口/VIP、备用入口、健康摘除/恢复与切换/撤销命令；独有测试 PG 的连接方式、主备/备份故障域、复制与数据丢失边界、可注入故障及选主/恢复/回滚命令；隔离测试数据、执行负责人及以上各动作的授权范围。凭据使用原有私密配置，不写进报告。在这些条件取得前仅保持材料就绪，不购买机器、不触碰真实业务库，也不用同机多进程冒充 A19 已验收。

### R04 实际候选安装包与必要 CI

[完整 CI 39901894609](https://github.com/fengyanweb/ku/actions/runs/39901894609) 实际 checkout `8abb9bc83cfe6b5c82fd16aa92a0f6d08c7dff39`：**16 个 job 成功、0 失败、1 个仅 tag 触发的公开发布 job 跳过**。质量检查、三平台完整测试、ASan/UBSan/leak、TSan、native、发布契约、三份真实安装包及完整资产集合均成功。工作区测试 Linux 2016 通过/7 忽略、macOS 2017/7、Windows 2010/6；忽略项不是通过。Windows PG 与真实宿主/Caddy 滚动及回滚检查也通过。重复 PR run `39901898676` 已取消，不计通过。公开发布没有执行。

此前归档只共同覆盖 version/hello/JSON，相关多循环安装验证仅有 Mac 独立记录。最小补齐为 `.github/scripts/export-release.py` 复用现有 `multiloop_load.py`，由**最终解包 CLI** 构建并运行已有 fixture：2 loops、3 秒、1rps，依次 `/ok`、`/cpu`、`/timer`，三个完整正文正确。HTTP/Task/计时记录和排队字节清零、drain 退出0，CLI 哈希与 manifest 相同。复用已有 POSIX session / Windows Job 子进程管理，180秒运行、1MiB合并输出与10秒清理上界；未改变资产格式、生产运行时或服务超时。32 项 exporter 回归、24 项发布契约检查通过。

| 8abb 实际归档（包内版本 0.0.18，experimental） | 字节数 | SHA256 |
| --- | ---: | --- |
| `ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz` | 47,760,113 | `2292809792f4c943434d7d5c036069b0027ddff6170fa3a93590d20f794e6e85` |
| `ku-v0.0.18-aarch64-apple-darwin.tar.gz` | 39,328,662 | `0c59820a3ce9d8579a376ae6e798b58ea552c92aee14eb1e517b9ea927a3fd20` |
| `ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz` | 43,495,840 | `20f3f6dcf2f88b40e3aa7b2ab98d887278b926ebef78e1cf3766fcff5037e8ac` |

三个归档实际下载后逐一核对 API ZIP 摘要/CRC、字节数、归档 SHA、提交/平台/架构、payload 与许可证；文件数分别274/270/260。各自原生 CI 中安装后 `version`、`check hello`、`run hello`、`run JSON` 和上述多循环程序均通过，临时解包、构建与进程树清理确认。归档、各 manifest 和收据在 `/Users/mac/Downloads/Ku_跨语言对照_2026-10-22/ci-assets-8abb9bc/`；原生日志及 CI 收据在 `/Users/mac/Downloads/mac-candidate/8abb9bc83cfe6b5c82fd16aa92a0f6d08c7dff39/`。这些是当前候选文件，不把 a2/752 旧归档改称 8abb。

a2 三平台基础安装通过、821 本机 Mac 多循环安装通过的原记录继续保留；旧失败、跳过与定向补验见后文。版本仍为未公开候选 `0.0.18`，完整 SHA/manifest 才能区分；本轮没有覆盖已公开 v0.0.18-r1，未合并、打标签或上传官网。

### T01—T08 执行完成与复现身份

永久基准及复现入口位于 [tests/benchmarks/cross_stack](../tests/benchmarks/cross_stack/README.md)，四栈服务、依赖锁、三种黄金语料、公共 Go 驱动及 cgroup 编排保留。先完成28个黄金窗口、224/224正确，再冻结协议/源码/生成C/ELF及依赖身份。实际完成 **12个场景、480个主表正式窗口，混合尾样本补充20个，共500个**；原96个五轮点为42合格、50失败、4个尾样本不足，补充4点为3合格、Node共同128rps点失败。执行完成不表示所有场景达到 SLO，也不是全部通过后挑出的成绩。

外部完整证据根目录 `/Users/mac/Downloads/Ku_跨语言对照_2026-10-22/`。`cross-stack-summary.json`（29,189,224字节，SHA256 `002ab35f307080119acce25592cfc76a817f261551507cf5f391e9602b91ec1f`）保留逐轮计数、p50/p95/p99/p99.9、CPU/内存/线程/FD及所有失败。Git 的集中 [evidence JSON](evidence/multicore-acceptance.json) 保存紧凑表和摘要，避免复制全部大数据。`raw/ku-raw-export-20261022.tar.gz` 保留9476个必要文件、原始JSON/CSV/事件/资源、构建与诊断证据，3,476,038,165字节，SHA256 `b48f826c4d90b88870ddc42791b2353700defd69e81ebfd523a3ddde7b04acc3`；`raw/export-index.json` 逐文件列大小/摘要，`raw/verification.json` 记录传输、gzip及逐成员核对。主表8380文件与补测前/后清单完全一致，62个唯一引用源文件的大小/SHA再次通过。

**同机资源与规则。** 宿主 M1 8物理核（4P+4E）/8GiB；专属 Apple VZ Ubuntu24.04 ARM64、8vCPU/4GiB、Linux6.8.0-142、无swap。不把VM的vCPU编号说成独占物理P核。服务整棵进程/线程树 CPU0至N−1，配额1/2/4核、1GiB；小内存组256MiB。Go负载端 CPU4—6/3核，每个驱动域1GiB；过载业务与独立探针是两个域，不能合称总共1GiB。PG16.15 单独 CPU7/1核/512MiB，1024行主键表、总连接池16。等待组 Python 协调器/受控peer同样在CPU4—6，但没有1GiB硬上限，不属于服务域。root Python 编排器也在服务域外。

固定 Go1.27.2、Node24.18.0、Rust1.89.0、clang18.1.3。Ku loops=N、全实例后台17（HTTP16+Task1）、线程预算64、Task64MiB、活动1024/pending128、连接4096、缓冲64MiB；handler45秒、请求整链30秒、idle5秒，等待组覆盖另列。Go GOMAXPROCS=N；Rust Tokio/Axum 使用正常worker/blocking配置；Node N个cluster worker及primary，成本含所有子进程和线程。实现不同但服务CPU/内存额度相同。具体完整变量与锁摘要以冻结 manifest 为准，SHA256 `cfebbfcdb15ffa6618cf169816dd66f32fc121ae8bcbd380bb977ba4e8176d46`。

负载为开放到达、共享LIFO空闲池256、一次连接一在途，500ms空闲到期在checkout检查；并发256、队列65536，期限为**计划时刻+3秒**，不重试、重定向、压缩或pipeline。CPU迭代数480/512/544/576k、seed1/3/7/11由请求驱动，512k/seed3校验22944003；没有缓存响应。Node整数中间值在精确范围。Ku的CPU响应含当前ABI所需的三个格式化helper Task，真实成本计入。W2对象1048字节、数组937字节、转义4116字节；所有完整正文做结构与类型校验。

Ku JSON/DB 经现有有界同步HTTP后台，子树取值的实际成本计入；native async直接DB仍不支持。Ku文本响应头为 `text/plain`，其他三栈为 `application/json`，正文语义相同但**不是完整HTTP头/API等价**。Ku PG连接归还执行 `DISCARD ALL`，Rust deadpool使用Fast回收并typed INT4读取；清理契约差异保留，不删除Ku reset来造优势。CLI不支持Linux ARM64自动链接，基准记录原拒绝后，对未修改生成C显式用clang18/C11/O2/pthread/libpq编译；这不是新增发布平台。

预热30秒、prime3秒，核心点各5轮×30秒交错。HTTP/JSON p99≤100ms、DB≤250ms、CPU≤1000ms、timer≤100ms；每轮计划正确率≥99.9%，正文错误必须为0。按计划时刻计延迟，窗口后完整结果不回填窗口goodput；平均负载端CPU达到配额90%、队列/直方图溢出等均截尾或判测量无效。V4用 CLOCK_MONOTONIC 锚点关联，误差≤1ms，Unix仅展示。10µs直方图合并原计数，不平均各轮p99；不足100样本不报p99，不足1000不报p99.9。下表仅列合并p99，**通过仍要求每轮及每类型通过**，所以合并值正常也可能判失败。

### 相同速率：完整调用成本与体验

表中的CPU是服务整个域在驱动存活观察期内CPU秒/最终正确数，包含同期失败与拒绝工作；峰内存是各轮 `memory.current` 观测峰值，非OS硬上限或进程总虚拟地址。每轮完整计数、500/503/超时/未发出/迟到、p50/p95/最大值及RSS/PSS/anon/file/线程/FD见集中JSON和外部summary，不能只按下表“计划失败0”推断每轮尾延迟合格。混合原18rps点因CPU尾样本不足，表中明确换为预声明128rps补充点，原数据未覆盖。

| 场景／到达rps | 栈 | 五轮门槛 | 计划／正确 | 窗口正确rps 中位[范围] | 合并p99 ms | 计划失败% | CPU ms/正确 中位[范围] | 观察内存峰MiB |
|---|---|---|---:|---:|---|---:|---:|---:|
| N4/1GiB/HTTP/9 | Ku | 通过 | 1350/1350 | 9 [9,9] | ok:12.29 | 0 | 0.498 [0.482,0.507] | 5.11 |
| N4/1GiB/HTTP/9 | Go | 通过 | 1350/1350 | 9 [9,9] | ok:11.85 | 0 | 0.635 [0.623,0.725] | 8.03 |
| N4/1GiB/HTTP/9 | Rust/Axum | 通过 | 1350/1350 | 9 [9,9] | ok:11.99 | 0 | 0.269 [0.263,0.283] | 3.98 |
| N4/1GiB/HTTP/9 | Node | 通过 | 1350/1350 | 9 [9,9] | ok:13.5 | 0 | 1.568 [1.489,1.749] | 145.8 |
| N4/1GiB/JSON对象/36 | Ku | 通过 | 5400/5400 | 36 [36,36] | JSON:15.49 | 0 | 0.55 [0.502,0.861] | 4.85 |
| N4/1GiB/JSON对象/36 | Go | 未通过 | 5400/5400 | 36 [36,36] | JSON:14.22 | 0 | 0.754 [0.717,1.234] | 8.15 |
| N4/1GiB/JSON对象/36 | Rust/Axum | 未通过 | 5400/5400 | 36 [36,36] | JSON:15.7 | 0 | 0.313 [0.302,1.002] | 10.48 |
| N4/1GiB/JSON对象/36 | Node | 未通过 | 5400/5400 | 36 [36,36] | JSON:168.71 | 0 | 1.459 [0.789,3.158] | 159.21 |
| N4/1GiB/JSON数组/2304 | Ku | 未通过 | 345600/345600 | 2303.967 [2303.933,2303.967] | json_array:13.22 | 0 | 0.103 [0.1,0.113] | 6.62 |
| N4/1GiB/JSON数组/2304 | Go | 通过 | 345600/345600 | 2303.933 [2303.867,2303.967] | json_array:2.04 | 0 | 0.129 [0.128,0.156] | 10.89 |
| N4/1GiB/JSON数组/2304 | Rust/Axum | 通过 | 345600/345600 | 2303.933 [2303.933,2304] | json_array:1.96 | 0 | 0.056 [0.052,0.062] | 10.28 |
| N4/1GiB/JSON数组/2304 | Node | 未通过 | 345600/345600 | 2303.933 [2303.9,2304] | json_array:8.15 | 0 | 0.103 [0.093,0.142] | 197.33 |
| N4/1GiB/JSON转义/576 | Ku | 通过 | 86400/86400 | 576 [576,576] | json_escape:1.98 | 0 | 0.255 [0.249,0.281] | 7.75 |
| N4/1GiB/JSON转义/576 | Go | 未通过 | 86400/86400 | 576 [576,576] | json_escape:1.98 | 0 | 0.293 [0.278,0.333] | 14.04 |
| N4/1GiB/JSON转义/576 | Rust/Axum | 通过 | 86400/86400 | 576 [576,576] | json_escape:1.86 | 0 | 0.14 [0.137,0.143] | 10.58 |
| N4/1GiB/JSON转义/576 | Node | 通过 | 86400/86400 | 576 [576,576] | json_escape:1.91 | 0 | 0.238 [0.233,0.259] | 169.9 |
| N4/1GiB/CPU/HTTP/144 | Ku | 未通过 | 21600/21600 | 143.933 [143.7,143.933] | cpu:642.08 | 0 | 15.943 [15.832,16.776] | 6.26 |
| N4/1GiB/CPU/HTTP/144 | Go | 通过 | 21600/21600 | 144 [144,144] | cpu:5.03 | 0 | 3.284 [3.264,3.324] | 9.77 |
| N4/1GiB/CPU/HTTP/144 | Rust/Axum | 通过 | 21600/21600 | 144 [144,144] | cpu:17.68 | 0 | 2.799 [2.758,3.228] | 8.7 |
| N4/1GiB/CPU/HTTP/144 | Node | 通过 | 21600/21600 | 143.967 [143.967,143.967] | cpu:121.18 | 0 | 8.77 [8.702,10.08] | 144.88 |
| N4/1GiB/PostgreSQL/6758 | Ku | 通过 | 1013700/1013700 | 6757.9 [6757.9,6757.967] | db:3.46 | 0 | 0.137 [0.136,0.141] | 6.13 |
| N4/1GiB/PostgreSQL/6758 | Go | 未通过 | 1013700/1013700 | 6757.933 [6757.933,6757.967] | db:203.4 | 0 | 0.109 [0.106,0.129] | 19.93 |
| N4/1GiB/PostgreSQL/6758 | Rust/Axum | 未通过 | 1013700/1013700 | 6757.967 [6757.933,6757.967] | db:154.47 | 0 | 0.055 [0.054,0.062] | 14.22 |
| N4/1GiB/PostgreSQL/6758 | Node | 未通过 | 1013700/1013700 | 6757.933 [6753.6,6757.967] | db:52.88 | 0 | 0.123 [0.121,0.126] | 327.68 |
| N4/1GiB/混合(补充)/128 | Ku | 通过 | 19200/19200 | 128 [128,128] | JSON:4<br>db:4.74<br>timer:30.94<br>cpu:44.81 | 0 | 0.805 [0.795,0.809] | 5.23 |
| N4/1GiB/混合(补充)/128 | Go | 通过 | 19200/19200 | 128 [128,128] | JSON:4.06<br>db:4.63<br>timer:30.19<br>cpu:11.27 | 0 | 0.648 [0.639,0.655] | 8.6 |
| N4/1GiB/混合(补充)/128 | Rust/Axum | 通过 | 19200/19200 | 128 [128,128] | JSON:4.11<br>db:5.73<br>timer:33.18<br>cpu:11.17 | 0 | 0.309 [0.299,0.332] | 2.59 |
| N4/1GiB/混合(补充)/128 | Node | 未通过 | 19200/19200 | 128 [128,128] | JSON:4.8<br>db:5.74<br>timer:37.2<br>cpu:29.75 | 0 | 0.954 [0.92,1.531] | 172.21 |
| N1/1GiB/HTTP/144 | Ku | 通过 | 21600/21600 | 144 [144,144] | ok:3.7 | 0 | 0.351 [0.323,0.358] | 3.6 |
| N1/1GiB/HTTP/144 | Go | 通过 | 21600/21600 | 144 [144,144] | ok:3.92 | 0 | 0.262 [0.253,0.278] | 6.85 |
| N1/1GiB/HTTP/144 | Rust/Axum | 通过 | 21600/21600 | 144 [144,144] | ok:4.19 | 0 | 0.174 [0.171,0.199] | 8.08 |
| N1/1GiB/HTTP/144 | Node | 通过 | 21600/21600 | 144 [144,144] | ok:3.88 | 0 | 0.393 [0.381,0.416] | 37 |
| N1/1GiB/JSON对象/14745 | Ku | 通过 | 2211750/2211750 | 14744.867 [14744.8,14744.9] | JSON:1.78 | 0 | 0.058 [0.057,0.058] | 9.14 |
| N1/1GiB/JSON对象/14745 | Go | 未通过 | 2211750/2211750 | 14744.933 [14744.867,14744.967] | JSON:516.58 | 0 | 0.044 [0.043,0.046] | 13.54 |
| N1/1GiB/JSON对象/14745 | Rust/Axum | 通过 | 2211750/2211750 | 14744.933 [14744.933,14744.967] | JSON:0.82 | 0 | 0.025 [0.025,0.025] | 9.36 |
| N1/1GiB/JSON对象/14745 | Node | 未通过 | 2211750/2211750 | 14744.9 [14744.867,14744.933] | JSON:330.37 | 0 | 0.05 [0.049,0.052] | 77.34 |
| N1/1GiB/CPU/HTTP/31 | Ku | 通过 | 4650/4650 | 31 [31,31] | cpu:29.14 | 0 | 21.818 [21.724,21.909] | 4.46 |
| N1/1GiB/CPU/HTTP/31 | Go | 通过 | 4650/4650 | 31 [31,31] | cpu:25.19 | 0 | 7.302 [6.741,7.549] | 6.95 |
| N1/1GiB/CPU/HTTP/31 | Rust/Axum | 通过 | 4650/4650 | 31 [31,31] | cpu:17.43 | 0 | 5.847 [5.623,6.062] | 6.56 |
| N1/1GiB/CPU/HTTP/31 | Node | 通过 | 4650/4650 | 31 [31,31] | cpu:22.11 | 0 | 11.093 [11.024,11.799] | 30.07 |
| N2/1GiB/CPU/HTTP/72 | Ku | 通过 | 10800/10800 | 71.967 [71.967,71.967] | cpu:259.39 | 0 | 16.503 [16.423,17.464] | 5.05 |
| N2/1GiB/CPU/HTTP/72 | Go | 通过 | 10800/10800 | 72 [72,72] | cpu:8.91 | 0 | 4.559 [4.522,4.704] | 8.66 |
| N2/1GiB/CPU/HTTP/72 | Rust/Axum | 通过 | 10800/10800 | 72 [72,72] | cpu:8.86 | 0 | 4.072 [4.03,4.18] | 5.79 |
| N2/1GiB/CPU/HTTP/72 | Node | 通过 | 10800/10800 | 72 [72,72] | cpu:60.63 | 0 | 9.708 [9.513,10.442] | 84.49 |
| N4/256MiB/JSON对象/9216 | Ku | 通过 | 1382400/1382400 | 9215.933 [9215.867,9215.967] | JSON:0.74 | 0 | 0.097 [0.097,0.099] | 8.73 |
| N4/256MiB/JSON对象/9216 | Go | 通过 | 1382400/1382400 | 9215.967 [9215.933,9215.967] | JSON:0.72 | 0 | 0.084 [0.083,0.084] | 9.42 |
| N4/256MiB/JSON对象/9216 | Rust/Axum | 未通过 | 1382400/1382400 | 9215.967 [9215.933,9215.967] | JSON:1.91 | 0 | 0.039 [0.038,0.044] | 12.44 |
| N4/256MiB/JSON对象/9216 | Node | 通过 | 1382400/1382400 | 9215.967 [9215.933,9215.967] | JSON:1.69 | 0 | 0.08 [0.077,0.081] | 181.84 |

### 同 SLO：有限采样下的最高已验证点

这是**最高已验证采样下界**，不是数学最大容量。短探索受低速率偶发尾延迟、有限档位和驱动截尾影响；例如四核HTTP Go的15rps不能解读成Go只能处理15rps，更不能用1792/15宣传Ku倍数优势。“无合格点”仅表示本次冻结核心点未取得五轮合格。正式失败点与下一短测失败/测量截尾分别保留，不重跑到出现好结果，也不据256MiB组的较好成绩推导小内存更快。

| 场景 | 栈 | 最高已验证rps | 窗口正确rps 中位[范围] | CPU ms/正确 | 观察内存峰MiB | 正式候选／下一短测观测点 |
|---|---|---:|---:|---:|---:|---|
| N4/1GiB/HTTP | Ku | 1792 | 1792 [1791.967,1792] | 0.055 | 5.06 | 正式1792通过；短1920:SLO/服务未过 |
| N4/1GiB/HTTP | Go | 15 | 15 [15,15] | 0.613 | 8.41 | 正式15通过；短16:SLO/服务未过 |
| N4/1GiB/HTTP | Rust/Axum | 9 | 9 [9,9] | 0.269 | 3.98 | 正式832未通过；短896:SLO/服务未过 |
| N4/1GiB/HTTP | Node | 9 | 9 [9,9] | 1.568 | 145.8 | 正式74152未通过；短78460:SLO/服务未过 |
| N4/1GiB/JSON对象 | Ku | 36 | 36 [36,36] | 0.55 | 4.85 | 正式60未通过；短64:SLO/服务未过 |
| N4/1GiB/JSON对象 | Go | 无合格点 | — | — | — | 正式240未通过；短256:SLO/服务未过 |
| N4/1GiB/JSON对象 | Rust/Axum | 无合格点 | — | — | — | 正式28672未通过；短30720:SLO/服务未过 |
| N4/1GiB/JSON对象 | Node | 无合格点 | — | — | — | 正式960未通过；短1024:SLO/服务未过 |
| N4/1GiB/JSON数组 | Ku | 无合格点 | — | — | — | 正式3840未通过；短4096:SLO/服务未过 |
| N4/1GiB/JSON数组 | Go | 2304 | 2303.933 [2303.867,2303.967] | 0.129 | 10.89 | 正式16384未通过；短32768:测量截尾 |
| N4/1GiB/JSON数组 | Rust/Axum | 2304 | 2303.933 [2303.933,2304] | 0.056 | 10.28 | 正式16384未通过；短32768:测量截尾 |
| N4/1GiB/JSON数组 | Node | 无合格点 | — | — | — | 正式16384未通过；短32768:测量截尾 |
| N4/1GiB/JSON转义 | Ku | 576 | 576 [576,576] | 0.255 | 7.75 | 正式32768未通过；短65536:测量截尾 |
| N4/1GiB/JSON转义 | Go | 无合格点 | — | — | — | 正式26624未通过；短28672:SLO/服务未过 |
| N4/1GiB/JSON转义 | Rust/Axum | 576 | 576 [576,576] | 0.14 | 10.58 | 正式32768未通过；短65536:测量截尾 |
| N4/1GiB/JSON转义 | Node | 576 | 576 [576,576] | 0.238 | 169.9 | 正式960未通过；短1024:SLO/服务未过 |
| N4/1GiB/CPU/HTTP | Ku | 无合格点 | — | — | — | 正式240未通过；短256:SLO/服务未过 |
| N4/1GiB/CPU/HTTP | Go | 144 | 144 [144,144] | 3.284 | 9.77 | 正式1536未通过；短1664:SLO/服务未过 |
| N4/1GiB/CPU/HTTP | Rust/Axum | 144 | 144 [144,144] | 2.799 | 8.7 | 正式1664未通过；短1792:SLO/服务未过 |
| N4/1GiB/CPU/HTTP | Node | 240 | 239.933 [239.933,239.933] | 8.646 | 151.5 | 正式240通过；短256:SLO/服务未过 |
| N4/1GiB/PostgreSQL | Ku | 6758 | 6757.9 [6757.9,6757.967] | 0.137 | 6.13 | 正式11264未通过；短12288:SLO/服务未过 |
| N4/1GiB/PostgreSQL | Go | 无合格点 | — | — | — | 正式22528未通过；短24576:SLO/服务未过 |
| N4/1GiB/PostgreSQL | Rust/Axum | 无合格点 | — | — | — | 正式18432未通过；短20480:SLO/服务未过 |
| N4/1GiB/PostgreSQL | Node | 无合格点 | — | — | — | 正式16384未通过；短32768:测量截尾 |
| N4/1GiB/混合(补充) | Ku | 128 | 128 [128,128] | 0.805 | 5.23 | 正式12288未通过；短13312:SLO/服务未过 |
| N4/1GiB/混合(补充) | Go | 128 | 128 [128,128] | 0.648 | 8.6 | 正式7680未通过；短8192:SLO/服务未过 |
| N4/1GiB/混合(补充) | Rust/Axum | 128 | 128 [128,128] | 0.309 | 2.59 | 正式30样本不足；短32:SLO/服务未过 |
| N4/1GiB/混合(补充) | Node | 15360 | 15358.667 [15358.267,15358.733] | 0.195 | 305.94 | 正式15360通过；短16384:SLO/服务未过 |
| N1/1GiB/HTTP | Ku | 480 | 480 [480,480] | 0.205 | 3.61 | 正式480通过；短512:SLO/服务未过 |
| N1/1GiB/HTTP | Go | 480 | 480 [480,480] | 0.151 | 6.87 | 正式480通过；短512:SLO/服务未过 |
| N1/1GiB/HTTP | Rust/Axum | 7680 | 7679.8 [7679.633,7679.967] | 0.017 | 7.79 | 正式7680通过；短8192:SLO/服务未过 |
| N1/1GiB/HTTP | Node | 144 | 144 [144,144] | 0.393 | 37 | 正式240未通过；短256:SLO/服务未过 |
| N1/1GiB/JSON对象 | Ku | 14745 | 14744.867 [14744.8,14744.9] | 0.058 | 9.14 | 正式28672未通过；短30720:SLO/服务未过 |
| N1/1GiB/JSON对象 | Go | 无合格点 | — | — | — | 正式24576未通过；短26624:SLO/服务未过 |
| N1/1GiB/JSON对象 | Rust/Axum | 14745 | 14744.933 [14744.933,14744.967] | 0.025 | 9.36 | 正式32768未通过；短65536:测量截尾 |
| N1/1GiB/JSON对象 | Node | 无合格点 | — | — | — | 正式24576未通过；短26624:SLO/服务未过 |
| N1/1GiB/CPU/HTTP | Ku | 31 | 31 [31,31] | 21.818 | 4.46 | 正式52未通过；短56:SLO/服务未过 |
| N1/1GiB/CPU/HTTP | Go | 31 | 31 [31,31] | 7.302 | 6.95 | 正式416未通过；短448:SLO/服务未过 |
| N1/1GiB/CPU/HTTP | Rust/Axum | 31 | 31 [31,31] | 5.847 | 6.56 | 正式448未通过；短480:SLO/服务未过 |
| N1/1GiB/CPU/HTTP | Node | 31 | 31 [31,31] | 11.093 | 30.07 | 正式128未通过；短144:SLO/服务未过 |
| N2/1GiB/CPU/HTTP | Ku | 72 | 71.967 [71.967,71.967] | 16.503 | 5.05 | 正式120未通过；短128:SLO/服务未过 |
| N2/1GiB/CPU/HTTP | Go | 768 | 767.933 [767.8,767.933] | 2.524 | 9.15 | 正式768通过；短832:SLO/服务未过 |
| N2/1GiB/CPU/HTTP | Rust/Axum | 72 | 72 [72,72] | 4.072 | 5.79 | 正式896未通过；短960:SLO/服务未过 |
| N2/1GiB/CPU/HTTP | Node | 72 | 72 [72,72] | 9.708 | 84.49 | 正式256未通过；短288:SLO/服务未过 |
| N4/256MiB/JSON对象 | Ku | 32768 | 32767.267 [32765.9,32767.667] | 0.088 | 9.93 | 正式32768通过；短65536:测量截尾 |
| N4/256MiB/JSON对象 | Go | 15360 | 15359.933 [15359.833,15359.933] | 0.077 | 14.34 | 正式15360通过；短16384:SLO/服务未过 |
| N4/256MiB/JSON对象 | Rust/Axum | 32768 | 32767.7 [32767.133,32767.767] | 0.035 | 13.6 | 正式32768通过；短65536:测量截尾 |
| N4/256MiB/JSON对象 | Node | 9216 | 9215.967 [9215.933,9215.967] | 0.08 | 181.84 | 正式32768未通过；短65536:测量截尾 |

### 混合持续、过载与资源回收

主表原混合18rps的CPU总样本25、Rust own30rps为45，p99不足。预先补充共同128rps、各5×30秒（每栈实际CPU样本190），只引用原own的20个窗口、不重算新增窗口；Ku/Go/Rust补充共同点合格、Node该点有单轮尾延迟失败。四栈最高合格采样点分别128/128/128/15360，所以按原60%规则向下取 **76rps**。原Node9216rps部分持续窗口的驱动无效/失败保留，未用短测或新窗口追认通过。

四栈各600秒、45,600/45,600正确、零终态错误，分类型p99通过：

| 栈 | JSON / DB / CPU / timer p99 ms | CPU ms/正确 | 正确但超过单请求SLO |
|---|---|---:|---:|
| Ku | 10.67 / 13.08 / 50.93 / 35.62 | 0.916 | 115 |
| Go | 8.14 / 9.66 / 15.12 / 34.03 | 0.804 | 92 |
| Rust/Axum | 6.98 / 8.96 / 15.66 / 34.82 | 0.352 | 20 |
| Node | 8.10 / 12.56 / 32.89 / 44.90 | 1.216 | 111 |

各栈CPU类型仅456样本，p99可用、p99.9不足；这次10分钟不替代长期可用率或原30分钟验收。服务前后20% `memory.current` 中位数：Ku5,160,960→5,271,552B，Go8,493,056→8,527,872B，Rust3,641,344→3,424,256B，Node264,978,432→280,633,344B。均有下降样本、线程固定；Node明显正向漂移约14.93MiB，现有主线程V8 heap中位数下降，不能唯一称为泄漏，也不能称长期恒定。末线程/FD依次23/22、10/23、6/22、39/150。

Ku最后在途采样尚有1个HTTP resident、1个DB pending/155B；真正quiet-before-freeze时HTTP/Task/DB pending、队列字节、缓冲全部0，连接池预期保留16。其他栈active/waiting归零，但未导出所有队列字节指标，不能补写0。最终全部正常退出，进程/线程/FD为0，80个自有cgroup删除、无清理错误。root Python编排器只采 `ru_maxrss` 高水位55,513,088→78,929,920B，不是current RSS、不是生产supervise协调器，不能证明后者当前内存稳定。

Go可取得GC +201次、GC CPU +1.9923秒、分配+472,956,008B/释放+472,837,904B。Node导出primary/HTTP主线程GC +1003次、duration +1594.82ms，没有CPU Worker独立GC/堆及完整分配字节；duration不是CPU时间或暂停分位数。Ku/Rust未提供全域分配次数/字节，不能推为0。完整趋势和原始作用域见外部 `resource-review.json`。

过载八组中七个实际640rps窗口均19,200正确/零错误；这只是相对Ku等有限合格点的5倍，**没有证明已真正饱和**，对Node更只是其15360合格点的约4.17%。Node相对自身的76800rps业务驱动无效：2,304,000计划=630,456正确+1,511,887排队未开始+70,143计划过期未开始+91,003超时+511明确503；sent674,362，队列峰65536、直方图超过100万桶。平均负载端CPU74.38%配额虽未达90%规则，仍不能挽救溢出的测量；这些超时不能全部归因服务。

独立普通探针有效，Node该组380/380正确，**原计划撤压**后首个新探针完成上界102.921ms，主要5秒恢复通过。业务发送尾部另延续2.355389秒，从实际最后发送计算的45.351ms仅补充视图，不替换主要起点。全部8组独立探针均在约1.5—103ms主要上界内恢复；普通业务探针不是独立管理通道可用性承诺。数据库均不重放。

### 等待容量与独立 batch

等待计划48档，实际37档：34通过、3失败、11按停止规则跳过。每档35秒timer、实际持有30秒，TCP由受控peer证实，普通探针55次不足p99，按**观测最大延迟**≤100ms门槛。Ku timer32/64/128/512通过且固定23线程；TCP32/64/128通过，512虽512个完整响应/实际持有30秒，普通timer探针132.317ms失败。Ku timer2048因既有1024 Task配置及64探针保留预先跳过，更高timer及TCP档按停止规则跳过；这些档位未实际发起，不改配置规避边界。

Go/Rust的timer及TCP均观察到8192档通过。Node timer32通过、64探针183.248ms失败；TCP32/64通过、128只有122正确/6客户端超时且实际同时hold0秒，后续停止。它们是本配置观测边界，不是产品硬上限。512 timer下Ku/Go/Rust服务观察内存峰6.86/16.84/17.12MiB、单核CPU0.569%/0.775%/0.393%；低内存收益须与CPU和更低已验证等待容量同时披露。

独立batch共80行全部正确：40个repeat0启动观察、40个repeat64计算，8个栈/核数组合各5轮。启动样本正确计算数0，CPU/正确写不可计算。单核计算CPU ms/op中位Ku2.198、Go2.382、Rust2.038、Node8.143；四核是**四个独立一核进程**，分别2.339/2.522/2.150/8.734，不是单Task多核。单核进程启动墙钟中位1.437/3.398/1.638/29.029ms；不从HTTP成本中直接扣除batch或启动，Ku内部毫秒计时分辨率也如实保留。

### T07 两个主要热点：证据、候选与退出决定

**证据。** 同速率 JSON（单核 14,745 rps）Ku/Rust 每正确响应 CPU 中位数为 0.05785/0.02482 ms；DB（四核 6,758 rps）为 0.13716/0.05547 ms，五轮比值 2.2—2.6。单核独立 batch 为 2.20/2.04 ms/运算，不能直接与 HTTP 成本相减。CPU 采样中 frame2_drive/frame2_check 自身占比 46.86%/9.16%；JSON/DB 的 HTTP loop 自身占比 19.65%/14.20%，wake 包含子调用占比 11.05%/14.62%，后两类不可相加。DB 保留 DISCARD ALL 清理契约；采样提交增量 1.903/0.953 次/正确响应受统计边界影响，不等于精确往返数。

**已撤回。** 状态选择候选两轮筛选未过全正确及波动门槛；HTTP owner 自唤醒候选仅筛选一次，六窗各 221,175 响应全正确，但 CPU 中位数仅降 6.57%、范围重叠，A1/B3 的 p99 未过。两项均已恢复，未带入生产；JSON 始终保留旧默认，不继续为选型追加优化。

**具体后续。** 一是基于已检查 IR 的热循环生成，保留取消、预算和清理检查，不泛化 AST 分析；二是 HTTP poll 活跃槽映射与 generation 生命周期，补复用、跨线程完成及取消回归，不据此重做 M:N。二核 CPU 120 rps 五轮在驱动结束后仍未过五秒静止门槛，并有 500，来源未唯一确定；该门槛不是产品承诺，也不重开历史七次 504 追查。

**采用建议。** 当前未建立通用 HTTP 吞吐或 CPU 成本优势。512 个真实 timer 等待均合格时，Ku/Go/Rust 采样服务内存峰值为 6.86/16.84/17.12 MiB，单核 CPU 占比为 0.569%/0.775%/0.393%，Ku 为 23 线程；省内存同时披露 CPU 代价。Ku TCP 等待最高全合格观测点为 128，512 的普通探测未过；Go/Rust 均观察到 8,192 合格。合格容量只是有限采样下界，失败点不是产品硬上限；不能宣传“零 GC 更快”或用容量采样比值证明迁移收益。

纯JSON库级辅助诊断也已完成：同一1048B对象、每阶段10000次、同进程预热后3轮、1核/1GiB，不含HTTP/Task/PG。Ku decode34—36ms/encode67—71ms；Go91.82—92.64/39.50—42.64ms；Rust13.80—13.96/7.59—8.43ms；Node13.29—14.64/16.05—22.17ms。低于20ms阶段按预声明规则标短样本，不算精确倍数或库排名；Node还含UTF8字节扫描，cgroup总CPU含启动/验证而阶段墙钟不含，作用域不同。旧JSON实现保持默认。原v1仅布置目录失败、没有测量，v2计划/源码/C/ELF/原JSONL及清理收据完整保留。

**新失败与未处理边界。** Ku单核CPU own52rps最后一轮、双核own120rps五轮未通过夹具的5秒静止条件，冻结原结果后仅继续缺少窗口，各自新进程/预热有记录；不是把同一代实例的五轮伪装成完整运行。单核该轮1560计划405正确/1155超时，结束仍有675连接、677 ready/resident Task；双核五轮18000计划中5958正确、237 HTTP500、2370 HTTP503、9435超时。四核CPU own240rps还有1034 HTTP500、4366 HTTP503、16607超时。500不是预期503；首批错误正文留存上限内没有该500的唯一内部原因码，不能猜成某一种预算错误或声称已修复。

源码边界确认：async/job连接在完整执行期间并非持续poll读端，客户端3秒断开不必立即取消活动Task；服务整链仍30秒、handler45秒。5秒静止是测试门槛，不是产品的即时取消承诺，也不足以把所有FIN当异常（半关闭合法）。6个失败代次经已有强制回收最终退出−9、进程0/cgroup删除；双核最后3轮驱动结束后仍分别消耗约13.822/13.281/13.998 CPU秒至清理，这些**未包含在表中驱动存活期CPU/正确**，不能把失败成本表当全部生命周期成本。现有1秒handler取消回归不能证明长CPU遇客户端断开就立即停止。

后续另立的范围是：为500保留有界原因码；围绕活动执行的客户端断开/半关闭/期限合同设计观测和取消；避免继续执行无用工作。当前无证据要求重做M:N，未触碰暂停的通用值复制/释放或AST分析。此处新CPU own场景不追认历史七次504；历史问题仍是“出现过、短测未复现、根因未确认、正式30分钟尚未通过”。本轮没有新证据确认内存破坏、错误计算或数据损坏，不能把失败的压力SLO扩大成这些结论。

### 验证清理与数据库变化

验证专属 `ku-cross-stack` VM 的原始证据逐文件导出核对后，私有PG fast shutdown退出0（约0.242秒）、其他客户端0、原6个PG进程全部退出、端口关闭、专属cgroup空并删除；VM已正常stop/delete。删除前VM目录allocated blocks估计约10.16GB，仅是文件系统块统计，未测主机free变化，不声称精确磁盘回收。旧 `ku-memory-acceptance` 仍Stopped，目录身份及小文件摘要前后未变；未碰共享缓存、其他应用和主工作区。收据 `cleanup-vm.json`。主机已删21项自有重复解包、被8abb替代的旧a2归档及一次性脚本/结果，普通文件合计4,128,173,154字节；不把逻辑字节数当实际磁盘回收量。旧a2 manifest/验证/失败日志保留，当前三包及独有旧Mac包保留。具体范围记 `cleanup-host.json`；保留永久基准、必要回归、当前三包、原始压缩证据/清单及精简结果，不在仓库积累临时探针。

本轮没有真实业务数据库结构或数据修改，无migration/SQL升级/回滚；只有隔离VM中的1024行PG基准夹具，随专属VM清除。故障恢复和无重放证据复用已有有效验收，没有重复36次内存矩阵或旧30分钟。原主工作区87项未提交修改保留，收尾时重新核对status及diff摘要。

### T08 发布定位与剩余动作

**本轮核心、C01—C06及新增T01—T08已在上述边界内完成，可进入独立实验版本的发布流程。当前未建立足以凭通用HTTP/CPU性能吸引迁移的证据。** 小型timer等待存在可量化服务内存收益，独立batch与Rust接近；HTTP CPU状态机、JSON/DB完整调用及高负载尾延迟仍有代价。有限容量表存在低速尾抖动/驱动截尾，不足以作为通用排行榜。应作为实验运行时候选交付，用户按真实业务评估，不宣称成熟的全平台高并发或跨主机高可用。

旧v0.0.18-r1早已发布，本独立分支不是旧版漏发。本轮明确建议将PR #2作为**新的实验版本**候选进入发布；版本号须与旧0.0.18区分，建议下一开发版本0.0.19，不能直接复写旧标签。当前实际包是8abb/0.0.18 experimental，不能改名后声称已构建0.0.19。用户负责最终合并/版本号/标签及公开发布时，再按所选号更新版本输入并生成对应真实包；本轮未执行这些动作，也未上传官网。A19仅缺上一节列出的真实故障域环境与动作授权，材料就绪但未实测；Mac附加异常按原边界保留。

### 校验修复与历史失败（保留原状态）

公共驱动v1曾在窗口结束时丢弃仍在原期限内的计划，作废收据保留；v2修正收尾但worker独占连接导致超过500ms空闲而反复建连，只作为重连诊断；v3共享有界LIFO池黄金28窗224/224正确、28次dial，准备时又捕获15秒内墙钟回退约269ms；v4最终改CLOCK_MONOTONIC锚点和≤1ms误差界。旧窗口不用于主表，真实过期/队列或直方图溢出仍判无效，恢复起点始终保留原计划撤压+5秒。

推送 `a2d96a6fdff4c4de1bc4318a237424081c57cfe3` 的原 CI 39826397242 已真实完成三平台
release-bundle 和完整产物集合校验，不能与旧 skipped 任务混淆。Mac full-tests 提前失败于既有测试的
130ms 内须发出3段的调度假设：实际正确触发 TimeoutError。已有测试改为受控时钟/底层传输确定性证明
剩余期限8/5/2，并保留原真实socket完整读取；逐次续期的反例必须失败。仅测试改动，不改业务期限，
原 CI 失败日志继续保留。修正后的 [Mac定向CI39830634661](https://github.com/fengyanweb/ku/actions/runs/39830634661)
已成功，日志确认实际checkout为 `dce36c7fc6c7122ed26aa7c5ba19bfccc89da6ec`；24项验收通过，
部署契约通过，平台不适用项单列跳过。该定向结果不改写a2原失败，也不代表本轮重复生成产物。

同一a2完整CI的Windows失败不同：Rust库测试
`registry_server::tests::registry_v1_real_tls_publish_acl_idempotency_conflict_restart_and_race`
读取响应超过绝对期限，随后race线程join失败；829通过/1失败/2忽略。
该job的24项Python验收已通过，不能用Mac夹具修正覆盖此失败。
完整日志和失败摘录保存在 `/Users/mac/Downloads/mac-candidate/a2d96a6fdff4c4de1bc4318a237424081c57cfe3/`，
这项 Registry 失败在 752 的真实 Windows 默认测试中已补验通过：832 passed / 0 failed / 2 ignored，
包括原竞态和两个新增期限回归；a2 的原失败仍保留。

Windows失败另确认测试窗口不一致：该服务从accept起允许5秒，而raw TLS测试发送完才新建3秒读期限，
普通测试agent一直使用8秒操作预算。仅修 `registry_server.rs::tests`，产品代码逐字节未变：
每次操作从准备/连接前取既有8秒绝对D，TCP/TLS/发送/完整读取/关闭继承同D；
首发与唯一明确429后的独立冲突检查受16秒总上界约束，未知结果不重放。
原3秒parser/滴流回归不变。本机6项helper回归及原发布竞态单例通过；Windows实际补验如上。
旧3850ms dispatch慢因仍未知，不能将期限错配写成已经还原了旧请求的全部原因。

752 的[完整 CI 39841878595](https://github.com/fengyanweb/ku/actions/runs/39841878595)
实际为15个job成功、1个Windows失败及1个公开发布跳过；Linux/macOS完整套件通过，不能把整轮写成成功。
Windows新的失败发生在12个慢查询结束后的首个新读：109ms返回 `error:acquire_timeout`，
验收代码直接按成功JSON解析；它没有观察后续恢复。该轮先前的断连恢复仍为63ms通过，原操作未重放。
仅两份Python验收文件复用已有5秒绝对恢复窗口、最多8个独立新读，完整保留中间结果；
产品超时不变，异常正文或未知HTTP结果立即失败，不重放数据库操作。
[d88定向CI39847626702](https://github.com/fengyanweb/ku/actions/runs/39847626702)实际checkout完整SHA已核对，
Windows PG17.11/4loops及真实宿主检查通过：压力组7次acquire_timeout、5次execution_unknown；
后续新读125ms错误、125ms错误、63ms成功，共313ms小于原5秒。丢提交确认只落一行、未重放，
drain退出0、数据库连接归零、清理无错误；三个LOCAL SERVICE宿主真实注册，替换ready2812ms，停止及进程回收通过。
本机25项夹具测试为24通过、1个Windows ACL平台跳过；Windows实库结果单独以上述CI为准。
精简收据和原始报告在 `/Users/mac/Downloads/mac-candidate/d88bfbfd5398f0d69ccb43dc11f02711fcbc2bd6/`。
重复PR运行39841886547最终cancelled，不能算作通过；没有新增合并、标签、公开Release或官网部署。
