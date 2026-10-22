# 多核执行与并发容量：执行边界及验收口径

核对日期：2026-10-22（用户时区）。已完成实现及任务书指定的平台主验收：Linux 完整 CPU/等待/公平/过载，macOS 最终扩核/回退与受影响路径，Windows 受影响回归及数据库/宿主补验。**macOS 额外 991rps 业务压力仍未收口，不能宣称全平台全部压力门槛通过。** 比较基线固定为 `c1de5e3245bff62eb0b422d5c39a5212a1d1723b`；各实际源码、CLI、生成 C、native、夹具与驱动 SHA-256 见 `evidence/multicore-acceptance.json`，所有原失败和取消结果保留。

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
- 夹具全实例上限固定：连接 256、活动请求 128、pending 128、HTTP 缓冲 8MiB、Task 接入账本 16MiB；不是每 owner 一整份。固定 owner 的本地活动/排队份额仍可能先饱和，需记录真实接入/拒绝；不能只看全局总量推断所有循环均可接入。
- `KU_TASK_MAX_BYTES` 不是任意 Task 全堆计量，HTTP 账本不是整体 RSS；第三方库、分配器保留、线程栈另有归属。Linux cgroup v2 / Windows Job 的进程限制与这些账本不同；macOS 原生不提供对应隔离，外部 supervisor/代理也不自动处于限制域内。
- native async lowering 目前只接受已支持的 Task 原语和已知顶层 async helper。没有 PostgreSQL/MySQL/Redis Task 操作或通用同步调用桥；直接在 async handler 构造/调用此类 DB client 会在 native async 子集诊断处拒绝，不能解释成已异步执行。现有 DB handler 走同步 HTTP 后台；阻塞不能偷偷搬回事件线程，数据库不自动重试。
- 等待容量、CPU 扩核、热点公平、慢体连接、服务过载各自成账。35s timer 的专用 45s handler 期限仅用于等待夹具，不改变原正式持续夹具的 1s 期限；完整响应才计成功，发生器未发出/主动关闭、服务拒绝、传输失败和结果未知分别保留。

## 最终源码与固定验收

生产修复冻结于 `245b9afe22a0eeb7b25a7d9a79e699cb75ae77a0`，容量驱动在 `461b46aa4474206f727afe06a9ac02be2a111bd8` 另行隔离等待组期限；两者间唯一差异为 `tests/acceptance/multicore_capacity.py`。专用 timer/TCP 等待组请求期限 45s，结束并退出该实例后，公平性、业务过载和连接组在新实例恢复原默认 30s。Go 客户端期限始终 3s。原持续夹具的 handler 1s、timer 25ms 和 50rps 普通负载未改动。

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

Linux 开放业务压力冻结为802rps，30s计划和实际均24,060：4,163正确、19,897明确503、其它/传输/未完成/未启动均零；服务全流量计数另含独立probe，保留下界及差额。撤压恢复上界1.311784s。Mac 原CI预检99.0667rps在2s窗口计划199、实际198且全正确，最后计划仅距截止1.346ms，小于该样本调度延迟p99=1.9ms，所以严格门槛失败、没有启动过载；原run保留失败。预检现在先向上量化为100rps，严格未启动零门槛不变；本机补验200/200正确，30s的991rps全部29,730次发起，7,302正确/22,427明确503/1次EOF，独立probe另有1次reset；该旧补验仍为not_closed，逐连接关联另外记录。

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

保持现有默认 1 个循环。计算负载可在本次固定预算内选择 2 或 4，先按实际硬件和 workload 量测；I/O 等待容量与事件线程数不同，不为等待自动增加后台总线程。这里没有证明 32 循环、万级同时活动请求、任意长同步指令可抢占或所有数据库操作可进入 native async handler。高于 `FD_SETSIZE` 的 macOS socket 仍正确回落 poll，但其性能没有由负向探测短测证明。

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

Windows 原整轮的具体失败是故障后第2个新请求 HTTP200/`error:acquire_timeout`，原日志只显示JSONDecodeError。源码中100ms acquire期限同时约束新连接建立，并未保证第二次请求必成功；这没有证明生产退化。修正验收为既有5秒恢复窗口内最多8个独立只读新请求，每次原期限，只允许已定义的query_error/execution_unknown/acquire_timeout作为中间状态，未知HTTP/传输/JSON仍立即失败，完整expected必须返回。原始故障/错误计算入记录，写入不重放。该次定向实际只发两次，62ms恢复。

## Mac 附加压力的剩余边界

本机未插桩30s补验保留为`not_closed`：业务1次EOF尚未逐条关联。随后单次有界关联诊断只证实一个独立probe reset：同TCP连接的前次200完成，服务按既有压力策略在HEADER/无部分数据/无Task状态关闭，客户端46.545ms后才复用；这不能自动解释原业务EOF。诊断构建另出现138次完整响应头等待超时，保留4个样本均为新连接，不能扩大成138个已逐条确认，也不能冒称HTTP504或全部归为idle回收。该构建修改了临时结构和时钟/连接元数据，具有测量扰动，不用它推导未插桩性能比。

最后只做一次固定6s的连接生命周期短采集：5946次全部启动，1693正确/4253明确503、业务异常0，另有1次独立probe连接错误；未取得≥2.5s异常生命周期，未触发线程采样。它不能代替30s门槛或覆盖旧失败。`accept→header_complete→async_submit→response_prepare→write/close`中的submit只表示提交，不伪称首次执行；内部原因未定位。所有追加采集均已结束，未放宽gates、未增加timeout、未重试请求，也未据猜测修改生产代码。

因此当前只交付已验证的Linux完整压力范围与Mac计算/等待/公平范围；**不承诺Mac在991rps该超载条件下无传输/期限失败**。后续若需要承诺该平台此类超载行为，应先取得未插桩异常的请求阶段和线程证据，再决定最小修复，不重跑无关长测或重做调度架构。

## 修改范围与清理

生产变更集中于`c_http_runtime.c`（bounded gather写回、Mac零期限负向探测）、`c_task.rs/c_task_adapter.rs/c_task_driver.rs`（64-dispatch量子证明与有界交接）、`c_task_reactor.c`（空网络扫描锁）及`c_task_http.rs`（私有LIVE负向readiness）；保留现有所有权、取消与最终回收协议。对应永久回归及原scheduler probe计数适配，测试SDK宏冲突/CRLF夹具修正不影响业务API。验收driver/夹具与本报告是任务书要求的永久复现材料，没有新增生产依赖。

数据库没有新增/修改/删除或migration；网站、v0.0.18标签、JSON与两个暂停项未动。原工作区87项修改的状态及tracked diff SHA与开始时相等；自有临时探针、生成C/Go/native、副本、测试目录与进程均清理，必要有界失败证据保留于报告根目录。实现工作树保留供PR审阅，不是垃圾副本；主工作区不自动覆盖或reset。最终只更新代码分支与PR说明，未合并、发布或部署官网。
