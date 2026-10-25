# 2026-10-25 性能架构实现记录

本页记录本轮内部实现与最终组合验证的边界，不代表正式性能、容量或发布验收完成。生成代码、真实二进制及原始窗口按提交和配置分别保存；历史失败窗口不改写为通过。

## 已实现的结构

- **A：typed Task 计算区。** 已验证的纯 Copy 标量区域使用局部值和有界原生循环，在返回、错误或非纯边界物化状态。已证明的私有线性循环可生成小型独立 C helper；只有不变边界、唯一单位正步长及无持久 header 写入/清除的形状使用预计算次数。零次循环、条件为假的 header、实际路径计费和初始化位保持完整；其他形状保留原检查路径。不按函数名、URL 或基准常量特化。
- **B：私有执行段与计算预约。** 只有完整 adapter 资格证明允许纯 continuation 保留 executor。HTTP 在完成必要 I/O 机会后使用私有 slice；真实 WAIT、显式 yield、host、owned、错误与清理仍退出到原有协议。预约的是容量，结算的是实际执行前缀，每次实际尝试至少计 1 credit，其余按 `ceil(states/64)` 结算；回调次数与逻辑 credits 分别计量。
- **提交索引。** driver 持有非 owning 的 Control 指针 key 集合，在原发布锁内查找、插入与回移删除；预期查找较短，最坏碰撞遍历仍有 `2 × capacity` 上界，不承诺最坏 O(1)。Control ABI、原 ticket/代次及唯一性校验不变，内部 Driver ABI 更新为 12。索引在逻辑退役、释放 Control 前删除；延后的物理退役回执只归还原槽位与费用，不删除后来复用同地址的新 key。
- **D：owner 持久事件集合。** Linux 使用 LT epoll，macOS 使用 kqueue；兴趣增量更新、ready 集合和 idle 期限堆替代每轮全量 idle 扫描。事件校验完整 token、连接代次与 owner；外部关闭、FD 复用及 macOS 双 filter 的旧注册回收有对应回归。Windows 保留 WSAPoll 路径，不据此宣称获得相同优化收益。
- **E：正常构建与合理对照。** 优化通过普通 CLI 生成实际 native 产物。Rust 参考保留历史模式，并提供有界合作取消、实际资源归还记录、显式监听 backlog 以及较少线程的配置；不同拓扑、插桩与未插桩结果分别记录，不拼成虚构的“最优 Rust”。

## 执行边界

| 入口/范围 | 当前约束 |
| --- | --- |
| 原始生成 frame、公开 `pump(1)` | 原 64 状态 bank；`pump(1)` 至多一次实际 poll，不借用后续 bank |
| 普通公开选择 | 最多 32 credits；B2 单次回调最多 4 个原 bank |
| 私有 HTTP 选择/预约 | 最多 128 credits、8192 状态、65536 纯操作，并受调用者剩余额度约束 |
| 私有 phase chunk | 同时不超过 256 状态和 1024 操作；单个可预约状态仍不超过 1024 操作 |
| 整个私有 slice | 最多 4096 credits；共用不可续期的起点及 `min(origin+1ms, external_deadline)` |

时钟域与真实读取保留，预约进入、结束、非纯及既有 fallback 边界仍读取原时钟，读钟后重新观察 phase；没有全局时钟缓存或新增监测线程。1ms 是协作式预算，可能被一个有限计算区或昂贵操作超出，不能解释为硬实时保证。

数值证明从本次调用的真实入口事实覆盖完整 8192 状态，不在 bank/chunk 之间重启旧证书。`MAX_WORK=8_000_000` 是一次 Task 程序生成中共享的**编译期证明工作额度**，不是运行时内存或 Task 预算；证明耗尽仍返回检查路径。当前算法只保留两层区间，不保存 8192 层。1M 版本曾使默认计算 helper 消失；只增加该工作额度后恢复，不能把两个不同生成路径混作同一预约参数的性能对照。

公共固定费用 helper 同时计算 driver、槽位、ring 与 `2 × capacity` 个指针 key；索引只在初始化时分配，不给每个槽位附加 hash cell，也不在每次提交时分配。当前 HTTP driver 容量 2048 对应 4096 个 key，即 64 位平台上 32KiB 的数组载荷；CLI root 容量 1024 对应 16KiB。它们是固定账本费用，不等同于分配器开销或整个进程 RSS。root 原有漏计已改用同一 helper；23 组其他既有 Task 夹具及 native driver 的精确预算/ABI 断言同步迁移。

## 初始化失败与资源归还

HTTP 工厂原先在 driver 处于 `REAPING` 时直接返回 NULL。真实故障注入的连续三轮分别残留 5/10/15 个分配，累计 1,047,336 字节（约 1.05MB）；这条无 worker 的部分初始化路径已改为先尝试销毁已记录的资源，成功后释放 backing storage。索引分配失败仍保持全零、可重试；普通分配失败、ZERO 状态与能够清理的部分初始化错误仍返回原有错误/Result。

另已实证：可选 blocking pool 创建真实 worker 后超过原启动期限，工厂返回 NULL 会丢失后续清理 owner，遗留 7 个分配、605,416 字节。此路径正按既有 native HTTP 清理失败的终止码 80 策略收口，不增加截止、不新增 registry：若原期限内仍未物理回收本机 worker，就终止 native 进程，不能宣称进程内恢复，也不能宣称所有错误返回行为都保持 Result。该修复的最终 HTTP 回归仍待完成。

## D2：连接 C、请求 A 与共享等待 P

`max_connections` 管理稳定连接记录与连接许可；空闲连接不占 A，也不预分配 header。首字节或已有 carry 触发请求接纳，取得 `max_active_requests` 的 A 后推进读取/执行；A 保留到响应完成且实际 Task/job 引用归还。关闭 socket 不会提前归还仍在清理的 A 或连接记录。Task、worker 和 job queue 容量仍基于 A。

原始等待 fd 与已有连接的新请求共用一个 `max_pending_requests` FIFO；节点按完整连接代次、请求序号或原始 fd 的队列代次识别，中途退出只退款一次。pipeline 后续请求也遵守 FIFO。排队、移交、部分读写不重置原请求/阶段截止；RAW 转为空闲连接时立即收紧旧截止，idle 堆也使用较早的原请求截止。等待 A 时暂停 READ，保持错误观察及必要的有限 SO_ERROR 检查，避免积存字节导致空转。

仅 A/P 压力不再挤断空闲 keep-alive；真正 C 记录压力仍可有界拒绝或回收。指标分别发布 `active_requests` 与 `peak_active_requests`。C/A/P 按 owner 分配，不等于全局可自由借用的 semaphore；零 C owner 不接受连接。解释器保留原活动连接策略。Rust 请求 permit 在返回 Response 时释放，与 Ku 覆盖写回和真实退役的 A 边界仍有差别。

## 构建与已取得的证据

Linux ARM64 已通过本机正常 `ku build --backend c --release --output … source.ku` 路径生成并校验 AArch64 ELF；本轮使用实际记录的 Clang 18、Rust 1.89 与 Release O2。`aarch64-linux` 目标识别不表示任意交叉环境或数据库/TLS 依赖包均已完成验收；仍需匹配编译器、目标 ABI 和所用依赖。Fast/O3 是独立构建配置，未替换普通 Release 默认。

- 私有 8192 状态版本的 54 项相关 native 回归及 8 项 RangePlan 单元回归已通过，覆盖公开精确入口、实际前缀、原截止、错误/取消与边界退款。
- 索引改动后的 24 组既有 Task 回归已通过；其中已修正 6 处固定 ABI 字符串和 2 处 ledger 费用断言。这些是随内部 ABI/固定分配变化的夹具迁移，不能记成语言或业务缺陷修复。
- 较早 D2 快照的 45 项完整 HTTP 回归，以及既有回归中扩展的真实 RAW 等待 fd → C 移交/无首字节/原截止/真实 worker 归还检查已通过。本轮最终组合的 47 项 HTTP 回归尚待验证；这些回归不证明普遍容量、无全部缺陷或长期稳定。
- 私有 phase 放宽为 2048 状态/8192 操作的候选虽通过 54 项相关回归，ABBA 服务 CPU 比约 `0.99168`，未证明明确收益，已撤回。当前仍为原 256 状态/1024 操作，不把该候选记入默认优化。
- 隔离 D 改动的短时探索中，4 个未采样窗口的服务 CPU/全部正确请求为原路径 `115.35/113.34µs`、新路径 `103.73/105.58µs`，几何均值比约 `0.9152`；另有 1 个采样窗口只用于定位。它是固定混合负载的预约、选择额度与完整证明组合结果，不是单次纯计算耗时、单一参数收益、最终 D2 组合成绩或正式 Rust 优势。

本机证据目录：[性能架构整改_2026-10-25](/Users/mac/Documents/Ku/实施/性能架构整改_2026-10-25/)（本机路径，不是公开下载地址）。关键记录包括 `private-8192-causal-result.json`、`wide8-related-regressions.json`、`private-8192-analysis-budget-diagnosis.json`、`D2-raw-handoff-regression-result.json`、`commit-binding-candidate.json`、`commit-binding-factory-reproduction.json` 与 `same-origin-factory-cleanup-review.json`；目录内保留对应原始窗口、构建身份、采样 PC、C 与汇编。不得用采样占比或调用次数替代未插桩 CPU 成绩。

## 尚未验收的范围

最终同提交组合的 47 项 HTTP 回归、物理取消/回收、最终 CPU、必要 CI、正式 Rust 对照、容量与 1800 秒持续负载仍待验；包括长请求尾延迟与过载恢复。短测改善不能说明历史长尾已修复，也不能提前宣布整份任务书完成。

通用值复制/释放与 AST 后续分析继续暂停，JSON 保留旧默认；本轮不追加数据库自动重试、官网部署或完整 M:N/通用网络调度改造。
