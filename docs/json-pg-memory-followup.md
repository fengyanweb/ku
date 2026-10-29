# 0.0.19 JSON、PG 与固定内存续执行结果

[English](json-pg-memory-followup.en.md) · 2026-10-29

**本轮保留一项同能力固定内存改进；JSON、PG CPU 追平目标仍未完成。** 三个 CPU 候选均未取得足够端到端证据，已撤回。运行时仅在 `codex/v0.0.19-json-pg-memory`，没有合入 main、发布、部署官网或操作生产数据库。

## 核心结论

| 项目 | 已确认结果 | 边界 |
| --- | --- | --- |
| JSON 热点 | 实际生成 handler 密集执行，7,212 个 cpu-clock SELF 样本、零丢样，SELF 未解析符号为零；写字符串、UTF8、append 与复制是主要项 | 密集循环不是 HTTP 成绩，inclusive 不相加；异步/共享成本未完整归因 |
| JSON 候选 | 去掉未使用码点计算的机制成立；便宜 A/U 单对曾为146.180→138.814 µs/正确请求 | 五组未确认收益，`93a0a77` 撤回；旧 JSON 实现与默认引擎保留，新增字节边界回归保留 |
| PG 候选 | 位图减少空注册表检查的机制可行；正常异步通知确实存在一次释放唤醒4名等待者的放大 | 位图实测89条HTTP500；单通知CPU单对301.614→316.002 µs，无收益，均撤回；不宣称稳定变慢或89错误已修复 |
| 固定内存 | `ab051e8` 将每连接状态行缓冲256→64B；相同4096容量、23线程、3000个正确连接高水位后，memory.current减少507,904B，anon/RSS减少573,440B | 是局部同能力实测，不能推广到所有负载或宣称已低于Rust；不是降低配置容量 |
| 期限 fixture | 补accept/header/首body/错误阶段与耗时，先保存结果并join再断言 | 原100ms期限、40ms等待、400ms上界未改；历史main失败仍不能唯一还原 |
| Rust差距 | 已取得一次预先安排的JSON五组完整原始结果，但环境和发送时序明显变化 | 不能给出当前最终默认的稳定排名；PG默认/同reset最终比较未完成，未用历史分数补齐 |

最终产品代码 `93a0a77` 相比 `c9ce1dc`，后端仅保留状态行缓冲改动；PG/reactor/JSON运行时均无其他差异。暂停的通用Value复制释放、AST、A3/A4、yyjson未动。没有增加期限、降低检查频率或重试业务SQL。

## JSON 动态证据和候选决策

原采样使用与正常CLI对应的c9ce1dc生成C；实际 `encode(KuStruct___ku_http_request)` handler，owned body/query/RID逐次合法创建和移交，正常清理。20种变化输入包含对象、数组、转义、Unicode/emoji、NUL、数值边界；RID16与32字节单列。1,783,808次完整输出比较，15.003814线程CPU秒、15.552275墙钟秒；采样497Hz、DWARF8192，7,212样本、0丢样。匹配libc6-dbg2.39-0ubuntu8.9后SELF未解析符号为零；没有升级libc。

逻辑SELF取每样本首渲染帧（可含inline），分母7,212：

| 函数 | SELF样本 | 比例 |
| --- | ---: | ---: |
| ku_json_write_string |1188|16.4725%|
| ku_json_utf8_scalar |1088|15.0860%|
| ku_json_buffer_append |907|12.5763%|
| __memcpy_generic |657|9.1098%|
| ku_json_parse_string |615|8.5275%|
| ku_json_parse_value |400|5.5463%|
| ku_json_write_value |303|4.2013%|
| _int_malloc |206|2.8564%|
| memcpy@plt |126|1.7471%|
| encode |121|1.6778%|

首个非inline物理帧是另一种归属：例如write_string1204、append1044、parse_value480，不能与上表相加。原始perf、完整捕获链、PC→生成C/DWARF及反汇编都保留；符号可解析不等于每个阶段已归因。密集循环改变缓存、分配器和调度，不能从HTTP CPU中直接相减。

- 既有分配账不变：decode28/35次分配调用、申请7800B；encode4次、申请3264B；申请和不等于驻留或CPU比例。
- UTF8宽度候选：两个调用者不使用scalar，O2仍计算并写出；候选保留C2/DF、E0/ED/F0/F4、截断、错误优先级和清理规则。便宜HTTP A/U各15000正确、无错误，CPU点值约低5.04%，但内存点值高264192B，不能当稳定改善。五组后撤回，实际默认仍为旧逻辑。
- 容量提示候选：根容器字段数≥2时首次64→128。76输入字节/语义一致、349个OOM点清理恢复通过；冻结对象4→3次分配，但小对象峰值64→128B，大单段申请反而增加。已否决，未进入默认或HTTP计分。无完整二次遍历、永久大缓存或借用短命输入。

### 比较合同的具体差异

此次保留原夹具，没有为同工诊断改写路由。`/json` 的decode→counter+1→移交data→encode业务与结果语义一致，HTTP均为1.1 keep-alive；这不表示HTTP头、封装和API成本完全相同：

| 项目 | Ku默认 | Rust默认 |
| --- | --- | --- |
| JSON响应Content-Type | `http.text`：`text/plain; charset=utf-8` | `encoded`：`application/json` |
| JSON执行封装 | 有界同步HTTP handler，16个同步worker；不是Task计算路由 | Axum异步route中调用serde_json |
| JSON拥有值 | owned body/RID、KuValue及take子树 | owned body/RID、serde_json::Value及remove子树 |
| PG客户端/结果API | libpq文本参数及结果，checked owned-result `value_int`、UTF8验证 | tokio-postgres typed INT4/String API，类型化参数/结果 |
| PG默认归还 | 原期限内完整`DISCARD ALL`并验证，失败驱逐 | deadpool Fast，不执行同样reset；同reset模式另列且本轮未计分 |

没有保留两边每个动态响应头的完整逐项抓包，不宣称头部字节完全一致。上述差异不能从服务CPU中凭空扣除；本轮既未达成稳定比较，也未将默认Rust PG当严格同合同成绩。

## 正常构建后的五组结果

A为复用的正常c9ce1dc CLI Release产物；B为已撤回的UTF8宽度＋first64组合；R为Rust默认。helpers/text/async/int、C11 O2、clang18.1.3、相同协议/输入/数据库/资源限制，500/s、预热10秒、计分30秒，每窗新实例，前一实例自然退出并移除域后才启动后一实例。顺序A-B-R、B-R-A、R-A-B、A-R-B、B-A-R；不是并行语言测试。无探针、采样、诊断计数或O3。

服务CPU µs/正确请求，每窗15000正确、驱动valid标记为true，零正文/传输/HTTP错误：

| 组 | A | B（已撤回） | R |
| --- | ---: | ---: | ---: |
| 1 | 239.825 | 123.593 | 97.745 |
| 2 | 228.594 | 241.517 | 97.145 |
| 3 | 141.643 | 241.834 | 103.344 |
| 4 | 119.396 | 232.378 | 102.544 |
| 5 | 147.438 | 499.484 | 351.173 |

全部225000正确。配对窗口几何均值及bootstrap描述区间：B/A=1.4371 [0.7928,2.4020]，B/R=1.8836 [1.4642,2.3821]，A/R=1.3107 [0.7268,2.1476]（20000重采样，seed20261028）。窗口独立/平稳假设明显存疑，区间不是稳定因果估计；不能据此宣称稳定变慢、追平或固定语言排名。B也不是当前最终默认。

完整/附带`/ok`守护各60000正确，CPU94.064→120.051 µs；未通过预期的3%点值守护。CPU守护A在计分中被协调器主动停止，PG最终比较未开始；保留中断和清理记录，不标为产品故障或通过，不替换窗口洗成绩。完整验收未通过。

## 环境：已确认、支持与未知

本轮没有启动任何宿主Node程序；每个完成窗口仅有一个受测guest服务、一个Go驱动、必要私有PG，前一服务已退出，没有冻结其他语言服务或重叠构建。服务4核/1GiB、驱动核4–6/1GiB、DB核7/512MiB；guest实际约4.093GB、宿主8GiB，不能相加。

宿主监测确实出现多个本轮之外Node进程高CPU和持续换入换出。JSON第5组B的约32秒宿主记录换入1,020,723,200B、换出1,447,165,952B，观测Node总RSS峰值约946MB（可共享，非精确私有内存）；同时guest可用内存最低约3.50GB、无guest交换空间、服务/驱动major fault增量均0。不能把宿主换页说成Ku自身被换出，也不能把minor faults一概当交换。

该窗驱动发送延迟p99桶337.92ms、最大1023.338ms；服务正确完成延迟p99桶481.28ms、最大1197.564ms。所有JSON valid_driver标记仍为true，它证明准备/核算通过，不证明发送时序和环境平稳。环境影响有支持，但具体线程等待、哪些宿主页被换出、每次慢请求由谁导致仍未知；不把89个PG错误洗成环境无效。

已停止余下同环境计分，未重跑七语言、历史504、30分钟或36次内存恢复。下一决定需要：可重复且发送时序稳定的隔离窗口，或足够的宿主/guest调度证据区分当前成本漂移；现有数据不足以将某个CPU候选作为默认提速方案。

## PG 最小成本表与撤回

| 部分 | 已确认的动态/调用证据 | 尚未知 |
| --- | --- | --- |
| pool | 独立诊断3000次claim：复用2992、创建8、enqueue97、释放健康3000；create→claim粗墙钟合计1148ms、max175ms（含调度/建立，非纯排队） | 锁成本、精确队列等待与阶段CPU |
| query | 3000次业务send；materialize9000次，3000次扩容让出，校验/复制各78451B；result/cell/byte各3000次分配或扩容 | 逐阶段CPU及扩容成本占比，不能按调用数代替CPU |
| reset | 3000次DISCARD ALL提交、3000次terminal成功、0失败；相同lease和原期限，收尽并验证后才归还 | reset独占CPU、业务/清理的共有libpq成本 |
| 共有libpq/Task/reactor | 旧1142样本：服务335、kernel581、libc155、libpq46、vdso25；44条SELF alias来自Task-net；新计数3247次NEW，旧扫描10088429个槽且0别名退休 | 旧采样受扰、不能作为当前精确CPU占比；query/reset与内核/调度共有成本无法唯一拆分 |
| 数据库 | 独立DB域/CPU/时间窗有原始资源记录，query与reset工作量单列 | DB SQL执行、网络/唤醒和后台成本未精确分账，不能算入服务CPU |

位图候选e142c3d在首次真实PG对照中出现89条HTTP500，全部`database_error:acquire_timeout`，驱动有效。710a288撤回，保留实验分支。完整失败快照仅12条：前4个保留快照为CONNECT，后8个为ACQUIRE且消费过14–25次通知；无完整线程CPU/栈证据，根因未知。没有以静态等价审查宣称运行正确，也没有凭宿主记录丢弃失败。

随后单通知候选dec9a47：一次归还实际wake/ready4→1；14个隔离旧新用例、候选20项永久回归通过（1实库opt-in未启用），包括真实interest交接、取消/过期/多空闲lease、128waiter关闭和callback pin。正常CLI N/Q各6000正确、驱动有效、回收正常；CPU301.6135→316.0018 µs，未显示收益，27d9d57撤回并保存实验分支。单对不证明稳定回退；通知放大也未解释89次错误。不恢复旧simple-query reset实验，不重试业务SQL。

## 固定内存实际改动

KuHttpConnection在Linux ARM64由1112→920B。相同4096容量申请少786432B；3000个正确连接高水位并关闭后：

| 指标 | 原first256 | first64 | 差值 |
| --- | ---: | ---: | ---: |
| memory.current |7426048|6918144|-507904|
| anon |6270976|5697536|-573440|
| RSS |12636160|12062720|-573440|

所有3000个响应正确，最后connections=0、线程仍23，自然退出/域回收通过。归一状态100..599，最长内置431原因31字节，完整行含NUL47B，64B足够；原截断/错误清理仍在。

固定表还包括：阻塞job+ring1540096B、Task槽638976B、reactor registrations596544B、hash524288B等，启动申请、对应owner/handler/Task物理结束后释放；完整尺寸/首次触页表保留。源级65536B portable poll snapshot已被O2删除，不能再认领为新节省。未砍线程、队列或连接能力；虚拟栈/申请量/RSS/cgroup分别计，不相互替代。

最终93a0a77正常Linux CLI产物再验证：`/ok`200、JSON200、PG100、CPU20，共520正确，静默/自然退出/域回收通过。完整.text与前述独立M内存筛查产物相同，功能验证不冒充CPU计分。CLI、C、ELF SHA与正常构建日志见证据。

## 检查、交付与剩余范围

- 最终源码Rustfmt、clippy/type check与diff检查通过；撤回JSON候选后20项JSON回归再次通过。状态行/partial vectors、3项metrics witness已通过；地址/未定义行为检测按实际候选/最终版本分别记录，Linux最终CI以本次提交Checks为准。
- 保留必要永久回归在已有测试文件；一次性探针、脚本、测试副本、临时构建与私有DB将统一清理。原始采样/失败证据保留精简包，不上传诊断脚本。
- 无schema/migration。两个暂停项未动，旧JSON默认未变；不承诺第三方库OOM均可转普通Result。
- 主工作区既有443文件核对：442不变、README另有修改、缺失0；新增LICENSE/NOTICE/README.en.md原样保留。开发工作区用户原有multicore文档SHA未变。
- **未完成：JSON/PG CPU追平、PG最终默认及同reset五组、四路径对Rust的正式内存目标、稳定尾延迟/完整性能验收。** 本轮只确认局部内存改进和诊断/候选决策，不宣称完整v0.0.19性能目标完成。

## 可移交证据

Git摘要：[结构化结果](evidence/json-pg-memory-20261029.json)。原始证据以本地交付目录`性能续执行_2026-10-29`及其自包含压缩包提供：report中英文、evidence-index、原始压缩perf/链/PC/C映射/反汇编、每窗client/events/resources/runtime/退出记录、环境对齐、候选/OOM/回归及最终功能收据。压缩保留解压后SHA，不含私有DB配置或URI；原始stack/宿主进程信息不上传Git。旧2026-10-28数据保持独立。必要CI链接和最终清理结果以交付收据更新。
