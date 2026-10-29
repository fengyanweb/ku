# Ku 0.0.19 性能整改结果（2026-10-29）

本轮完成了候选实现、相关回归和有界诊断，**没有通过最终性能验收，不认领追平 Rust 或驻留内存下降**。候选只保存在 `codex/v0.0.19-json-pg-memory`；未合入 main、覆盖标签、发布或部署官网，已发布默认实现未切换。

- **已确认**：旧结果分类会把 HTTP 500 放进 driver 无效分支；现已优先保留请求失败，分别记录功能、账本、发送、环境及候选采用状态。迟发不再自动被解释为外部故障。
- **有支持、未确认端到端收益**：同一 1048 B、16 位 RID、owned 输入和实际 `encode` handler，O2 无采样密集点值 A **5.188 µs**、独立 J1 **4.538 µs**、J1/J2/J3 **4.362 µs**。这不是 HTTP 成绩；不把内联后消失的函数符号解释为验证成本全部消失。
- **已确认、范围有限**：PG 20 项、JSON 21 项及相应 ASan/UBSan 检查通过；真实异步 HTTP→PG 的万行结果、超时、断连和不确定写入恢复通过，无 SQL 重放。A/B 各三轮 512 个真实网络等待、1536 条精确响应，资源和预算回到各自 ready 基线，正常退出。
- **未达采用条件**：固定账少 458,752 B，P1 索引预留多 131,088 B，合计账少 **327,664 B**；最终 anon 相同，B RSS/PSS 各高 4 KiB，观测峰值高 430,080 B。不能认领 M 的 RSS 收益。
- **正式矩阵阻塞**：三次 30 秒 A 控制均 15,000 正确，CPU max/min **1.105059 > 1.10**，其中一窗 scheduled→attempt p99.9 **12.4ms > 10ms**。按预先规则停止五组 A/B/R。另一个同 ELF 诊断三窗 CPU 为 **106.844 → 136.571 → 165.096 µs/正确请求**，后窗发送质量失败，不能稳定排名或仅归咎采样。

| 工作项 | 实现提交 | 正确性 | 自身 A/B | 同合同 B/R | 内存／尾延迟 | 判定 |
| --- | --- | --- | --- | --- | --- | --- |
| JSON 完整请求 | 7ba3064、7aec348、536242d、f6f0bdd | 21 项＋ASan/UBSan；端点全部正确 | 3秒诊断 136.590→112.706 µs；不计正式分 | 未运行正式比较 | 尾延迟受发送漂移影响 | 阻塞，未采用 |
| PG 各自默认 | 00cfa41、efefff5、bd9d644、9ab85b1 | 20 项＋实库生命周期 | 304.462→231.438 µs，600条/窗，仅诊断 | Rust Fast 固定身份，未计分 | 小样本 p99.9 未知 | 阻塞，未采用 |
| PG 严格 reset | 同上，原 DISCARD ALL 保留 | Ku 生命周期及清理通过 | 沿用该 Ku 合同，不重复计分 | 严格 Rust 模式未正式运行 | 未建立统一尾部区间 | 阻塞 |
| /ok、/work 守护 | 组合 B | 正文全对，均自然退出 | /ok 24.009→26.381；/work 2791.963→2979.192 µs | 未正式比较 | 点值超3%，波动下不能判稳定回退 | 未通过采用门槛／原因未知 |
| 固定内存／布局 | 1e8fc46，含 P1/P4 的组合 B | 3轮高水位及回落通过 | 合计账 −327,664 B | 不作语言内存排名 | anon不降，RSS未改善 | 驻留目标未达标 |
| 失败候选保护 | f2cd0e2、00cfa41、9ab85b1、bd9d644 | fd复用、取消、关闭、OOM、预算、拥有值回归通过 | 不适用 | 不适用 | 有界异常证据 | 已完成相关验证 |

## 实现与影响范围

J1 是独立的 UTF8 width-only 校验；J2 使用完整可读 8 字节的普通 ASCII 探测，非 ASCII、引号、反斜杠和控制字符仍走原语义；J3 将 reserve 慢路径与 1/2/6 字节写入分开。原预算、错误优先顺序、转义、深度及释放约束保留。本轮没有证明 JSON 请求级分配次数下降；没有引入无限缓存。

P1 为每个 owner 增加精确 fd→registration 索引，保留 fd/generation、kernel ADD 失败回滚和旧别名退役规则。最坏查找仍有容量界限，不宣称无限制 O(1)。本资源合同索引总成本是131,088 B。

P2 将小 PG 结果的 header/cells/bytes 从 3 次分配、440 B 请求量改为 1 次、448 B；大结果按原规则增长，内联尾部仍存活时按真实占用计账。P3 仅对能装入剩余字节/单元格量子的 compact 首行取消一个多余让出点；在该处重新观察 job 取消、control phase 和原绝对期限。保留64 KiB/64 cells/32 protocol 限额、裸 API 和外部增长的原让出行为。新增短锁、phase 原子读取和时钟成本仍需端到端判定。

P4 固定保留首个、语义状态变化、最近失败，记录覆盖及丢弃计数，不在 HTTP 热路径打印。实库捕获了三个冷连接 slot 的 enqueue≤start≤finish；预热连接及尚未提交 worker 的记录保持0/缺失。历史位图实验的89次失败仍根因未知，不能据此认定原默认有同一缺陷；失败位图和单通知实验均未恢复。

M 把9个动态 KuValue 配置位改为3个拥有字符串、5个整数及布尔/存在位。实际 NET+PG、FILES/DIRECTORY 关闭时 job **744→520 B**；NET-only 回归中的496 B是不同特性集，不能混算。`fixed_bytes` 和 `reserved_bytes` 相互独立，应相加，不应相减。三轮 FD **1038→14**、线程始终21，末轮驻留各增加4 KiB；这既不是持续泄漏证据，也不足以排除长期泄漏。

## HTTP 与动态采样

同步 `/json` 不经过 `/work` 的计算 Task 状态机。全服务采样121个运行态 SELF 样本，lost0；owner76、sync worker45。采样窗 user266.313ms、system416.542ms；schedstat 运行时间 owner470.244ms、sync212.288ms，不能与重叠采样占比相加。

最大 SELF `__wake_up_sync_key` 为13/121：其中8个调用链来自 TCP 响应 loopback，5个来自 worker→owner pipe 通知。并非全部内部唤醒，也不等于 futex 等待墙钟。线程观察器在域外消耗约0.325s CPU、峰值RSS25,399,296 B，不计入服务；域外不代表没有调度扰动。Ku PC有生成C/实际ELF汇编映射；内核样本主要保留符号及偏移，没有由此推断锁竞争。未取得有效PMU计数，不提供IPC或cache miss。空闲服务只有约4.5–4.8ms CPU/10–11.4s，采不到 CPU 样本；不支持空转占据主要 CPU 的说法。没有证据证明通知冗余、锁竞争或具体可交付公共路径改法，本轮没有强行合并队列或删除交接。

密集采样 A3988、J1233996，lost0，保留采样器记录的调用链和实际 PC→C/汇编映射。A UTF8 独立 SELF742/3988；J123 仍有546/3996个样本落在内联 width 校验源码区。后者已包含在 parse/write 的函数归因中，不能再相加；不能把742个样本全称为被消除。带采样8CPU秒 A5.983/J1234.540 µs，与无采样1CPU秒点值分开保存，不混算收益。

## 环境、比较与未知项

实际环境是 Linux ARM64 VM4vCPU/3GiB，服务2core/1GiB，驱动CPU2一核/1GiB，私库CPU3一核/512MiB；HTTP2 owner、B17、4096连接、1024 active、128 pending、Task2048、PG池16。不同语言及服务严格串行，前一服务自然退出并移除域。配额不是物理资源预留，也不能与宿主 RSS 重复相加。

本轮宿主记录有真实 Swapins/Swapouts 增量，且出现约191–194% CPU、约496–499MiB RSS 的 lint node；RSS 含共享页。不能确定具体项目，不能证明 Ku 自己被换出，也不能把全部波动归给它。控制门槛在候选计分前固定，未为候选放宽。未知原因保留未知，不把所有迟发改标 measurement_invalid。

会话入口实际HEAD为7aec348（续执行期间已落J1/J2候选），比较A选最后可用默认988f32e，排除尚未验收候选；入口HEAD与对照A不是同一概念。基线988f32e已含 first64，它不是本轮收益。正常构建始终 CLI Release/C11 O2/helpers/text/async/int；JSON旧引擎、原期限和取消规则保留。未恢复 yyjson/A3/A4，未触碰暂停的通用 Value 复制/释放与 AST，也未重做M:N、七语言或36次内存验证。Rust参考 ELF SHA `66b1d69287ca7356951a86e32db696524e2049abf24c6437d1ab580fdbc443e5`；默认 Fast 与显式 reset-equivalent 合同分开，缺正式比较时不提供 B/R 比值。

下一步所缺的是满足既定发送与控制 CPU 门槛的同合同环境；随后才有资格运行最终五组矩阵和3%守护。若继续定位公共成本，区分 `ku_http_write_limited/sendmsg` 与 `ku_http_wake_notify/pipe` 的实际工作及零工作通知，保留取消/期限/清理；121样本不足以预设合并方案。不得从已知波动的短点值直接采用候选。

## 回归、CI 和清理

本机格式和 all-target clippy 通过；Python现有回归53通过；PG20、JSON21、reactor1、net owner1的相关ASan/UBSan检查通过。macOS不提供本次 Linux LSan 的同等结论；相应 Linux CI 检查另列。

首次CI `6bdfb05`：18通过、1失败、3条件跳过。唯一测试失败是旧指标期限夹具的40ms预睡被延迟到172ms，100ms D 已过且连接未建立；不能当成产品正文读取缺陷。`67ece0c` 提取原读取循环供已就绪 TCP 响应测试，产品继续传原绝对期限，没有增加 timeout。该模块11项、本机 clippy及格式均通过。最终CI状态见交付目录的 `ci-final.json` 及最终执行记录，首次失败不删除。

本轮无数据库 schema、migration 或依赖变更。私库与本轮 cgroup 已正常清理；VM恢复原8vCPU/4GiB且Stopped；删除本轮临时脚本、源副本及构建产物，保留必要原始采样、符号ELF、生成C和精简失败证据。主源码核对442文件未变，README是本轮开始前已有差异，0文件缺失；用户未提交的 multicore 文档SHA未变。

## 复现入口与证据

永久入口在现有 `tests/benchmarks/cross_stack/`：`focus_cycle.py`、`json_dense.py/.c.inc`、`http_cost_diagnostic.py/http_thread_roles.c.inc`、`host_context.py`，复用原V5驱动与 waiting/PG生命周期入口。保留最小入口是本次任务书明确要求，其他一次性探针已删除。

- 正常构建：`ku build --backend c --profile release --emit-c -o OUTPUT server.ku`；固定已展开fixture和 `KU_PG_LIB`，构建身份见ZIP内 `raw/candidate-identities.json` 及各版本 `normal-build.log`。
- 密集诊断：`python3 json_dense.py --generated-c NORMAL_C --output NEW_DIR --source-commit FULL_SHA --cli-sha256 CLI_SHA --link LIBPQ --debug-symbols --family http-object --context http --cpu-seconds 1`。默认不指定计时参数只做预检；其他字符语料分开列。
- 控制/筛选：`sudo python3 focus_cycle.py --plan PRIVATE_PLAN --out NEW_DIR --phase control --workload json_object --names A`。计划、阈值及每次失败均保留，正式资格未通过时不能启动 final。
- 实库：`python3 tests/acceptance/http_pg_lifecycle.py --ku CLI --lane async --profile release --loops 4 --output NEW_JSON`，只用该入口新建的私库。
- 三轮高水位复用 `waiting.NetworkPeer/Cohort`：同一服务512个 `/tcp` 挂起，观察真实 net_wait，释放K、核对全部响应，等待 quiet和原预算，重复3轮再退出；A退出后才启动B。每阶段原值在 `raw/layout-high-water-v2/*/observation.json`。
- 原始 PC、调用链、汇编与 SELF：`raw/dense-layout-{A,J123}/perf-*` 和 `raw/diag-thread-A-20261029/json-*`；完整原始perf在各采样目录。DWARF栈采集上限8KiB；文件无截断不等于任意深度调用栈都完整展开。inclusive链有重叠，不能加总。
- `analysis-results.json` 汇总结构化结果；ZIP内 `raw/evidence-files.json` 给出逐文件SHA。包内相对路径指解压后的证据位置，不要求保留本机绝对路径。

方法参考：[Google Benchmark 测量方差](https://google.github.io/benchmark/reducing_variance.html)、[LLVM 基准测量](https://llvm.org/docs/Benchmarking.html)。这些资料不证明 Ku 的具体根因。PG协议依据：[PostgreSQL16 异步接口](https://www.postgresql.org/docs/16/libpq-async.html)；本轮数据库事实以实际生命周期结果为准。

## 密集运行态 SELF Top10

同一类别中的分母分别为A3988、J1233996；每个样本只归原始leaf一次，含未展示的其余部分。

| 排名 | A 符号／样本 | J123 符号／样本 |
| --- | --- | --- |
| 1 | ku_json_utf8_scalar 742/3988 | ku_json_write_string 732/3996 |
| 2 | ku_json_write_string 584/3988 | ku_json_parse_string 651/3996 |
| 3 | ku_json_parse_string 290/3988 | ku_json_parse_value 227/3996 |
| 4 | __memcpy_generic 248/3988 | ku_json_write_value 218/3996 |
| 5 | ku_json_buffer_append 214/3988 | __memcpy_generic 184/3996 |
| 6 | ku_json_parse_value 196/3988 | _int_malloc 166/3996 |
| 7 | _int_malloc 168/3988 | ku_json_buffer_prepare 156/3996 |
| 8 | ku_json_write_value 155/3988 | __GI___libc_malloc 115/3996 |
| 9 | __GI___libc_free 110/3988 | __GI___libc_free 110/3996 |
| 10 | encode 96/3988 | _int_free 106/3996 |

最终代码CI：[67ece0c 的运行记录](https://github.com/fengyanweb/ku/actions/runs/40910009989)。采样B使用6c81803，至67ece0c的C backend、ABI及Cargo依赖无变化；新增私有指标读取函数不进入生成C服务。CI最终状态单独列示。

## CPU 计量边界与收尾成本

表中主 CPU 是驱动完整生命周期首末的服务 cgroup 累计 CPU 差／全部正确请求，不是1Hz到达窗插值，也不包含数据库、驱动或观察器CPU。完整生命周期还保留静默、drain及退出后的CPU，未把清理移出账本。短测B的完整口径依次为：JSON112.771、PG231.852、/ok26.403、/work2980.554 µs/正确请求，对应驱动口径112.706、231.438、26.381、2979.192；这些仍是不具正式资格的诊断点值。

3秒窗p99是保守桶上界：A/B的JSON1.55/1.50ms、PG2.72/2.41ms、/ok1.21/1.23ms、/work7.92/7.12ms；PG和/work样本数不足以给p99.9。不能拿这些短尾部代替正式30秒窗口，更不能代替长期稳定性。

最终CI已完成：67ece0c，19通过、3条件跳过、0失败（包括macOS完整测试）；跳过项与原始作业信息见ci-summary.json/ci-final.json。源码和性能采用状态与CI通过独立。

环境分层补充：三次30秒控制中的guest均SwapTotal=0，服务Linux major fault和CPU quota throttling增量均0，guest MemAvailable最小值见environment-layer-summary.json。这里没有观察到guest交换或服务域内存耗尽。宿主单个Renderer RSS峰约1.13–1.26GiB，lint node约496–499MiB；共享RSS不能相加为独占内存。宿主存在实际换页，但是否换出VM宿主页仍未知；guest major fault为0不能排除宿主层的VM页面等待。
