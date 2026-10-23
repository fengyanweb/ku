# Ku 性能与资源对齐：实施及验收记录

对应《Ku_Go_Rust性能对齐与资源领先_完整实施任务书_2026-10-23.md》。本轮仍在实施：已取得一组有效的七栈单核计算结果，尚未完成全部业务、容量及资源目标。**实现完成、测量有效、目标达成、安装交付分别判定。** 本文按结论与证据组织，历史失败不覆盖，未测项不填推测值。

## 1. 当前结论：哪里慢，已有多大证据

| 场景／身份 | Ku | Go | Rust | Ku/Go CPU成本 | Ku/Rust CPU成本 | 证据范围 |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| CPU HTTP／257c882 | 5.06465ms | 3.42885ms | 2.69045ms | 1.477 | 1.882 | Linux2核、1GiB、120rps×8秒，每栈960/960正确；带采样的单轮诊断 |
| JSON对象／b1ada03 | 75.27575µs | 73.32175µs | 55.62625µs | 1.027 | 1.353 | Linux2核、2000rps×8秒，各16000/16000正确 |
| JSON数字数组／b1ada03 | 79.95325µs | 201.89550µs | 56.62500µs | 0.396 | 1.412 | 同上；不能把某一语料外推为语言整体排名 |
| JSON转义文本／b1ada03 | 103.21131µs | 128.45650µs | 112.56788µs | 0.803 | 0.917 | 同上；不是正式七轮领先结论 |
| PG normal／257c882 | 107.436µs | 85.532µs | 55.071µs | 待同合同比较 | 待同合同比较 | 各16000正确；Ku含DISCARD ALL，Go／Rust normal不含 |
| 独立同步计算／f7e984a | 2.191016ms | 2.298799ms | 2.006278ms | 配对比见表C | 配对比见表C | 七栈各7个有效窗口；此行为完整进程CPU/计算的中位值 |

CPU HTTP 的完整服务成本差距明显；独立计算与不同JSON语料的差距并不相同。两者提交、输入分布和采样条件不同，**不能将HTTP减去batch来分配精确调度成本**。现有证据已定位到多次Task推进、算术／错误通道、时钟、通知、基础驻留及PG会话处理，但不足以给每一部分分配完整耗时比例，也没有证明必须重做M:N。

f7固定计算对Go的配对速度比为1.052587、95%区间[1.042989,1.060838]；CPU成本比0.951783 [0.944136,0.961433]，未达到≤0.90。对Rust的速度比0.946893 [0.911149,1.008926]，不能证明追平；CPU成本比1.066449 [1.005567,1.104857]，同样未达标。最新JSON／PG及同步快路径改动不在这批冻结结果中。

最新4ea已实编并完成三个独立短窗，均driver合格、自然回收、5秒夹具通过、cleanup为空：

| 4ea独立窗口，2核1GiB | 正确／计划 | 完整CPU／正确 | p99 | 范围 |
| --- | --- | --- | --- | --- |
| CPU HTTP，120rps×30秒 | 3600/3600 | 4.659532ms | 8.64ms | 单窗口，无同轮Go／Rust或旧源配对 |
| JSON对象，2000rps×8秒 | 16000/16000 | 65.4935µs | 1.70ms | 单窗口，非容量 |
| PG，2000rps×8秒 | 16000/16000 | 135.388875µs | 15.36ms | 单窗口；成本高于旧同步107.436µs诊断值，正在有界采样定位 |

该数据不能与257／b1跨时间、跨实现短窗相除当成单变量改善百分比，也不能冒填正式表A／B。原始目录为 `/opt/ku-bench/results/short-scenes-4ea1407-v1`；后续只补相关成本与有效对照。

PG后续第一次诊断在100rps×30秒预热中出现31个`database_error:acquire_timeout`（2969/3000正确），尚未开始perf，因此没有可用栈；同期driver调度最大延迟570.8ms，不能唯一归因服务或宿主。保留该失败，不自动重跑或延长产品期限。

为避免再次错过异常，第二次明确作为冷态、有采样开销的诊断，在第一条请求之前启用全服务cgroup软件cpu-clock 99Hz采样。2000rps×8秒16000/16000正确，p99 10.64ms；完整CPU2.042815秒（127.67594µs/正确，非正式收益），服务／driver／数据库均未节流。182个样本中，`ku_http_loop_run`有30个self样本，其中26个准确映射到内联`net_interests`整容量扫描的递增／读取job.state／比较NET_WAIT指令，约占全样本14.29%。实际ARM64槽跨度1200B，与原C的`i<pool->capacity`一致；这证明扫描是当前热点，粗粒度样本不能预测精确改善比例。其余包含网络、唤醒、driver提交等成本，self与children不相加。两owner贡献177样本、原blocking连接线程5样本，21线程均纳入同一域；自然静默2.27ms、活动账本归零、固定／reserved回到原基线、exit0、无强制退出且PID／FD及域均清理。原perf与C／ASM窗口保留在guest，未改变源、超时或正式合同。

## 2. 候选身份与测量合同

| 身份 | 用途／状态 |
| --- | --- |
| `cac47df6b1db6a3bbd4eebf470306221ca4ef13c` | 旧生产基线，生产路径与既有8abb9bc相同 |
| `1bdf88326461a11938f17609a618dbc27e4753d1` | 第一候选：Task热路径、HTTP活动索引／RST及旧JSON writer |
| `257c8827fbbe65b2a4b55dbea89e7e06de9e952f` | 早期集成候选；CPU／PG短诊断与V5，非最新候选 |
| `b1ada03bc5d0e7ae68d9ea8dbbc78e5c4da2ca71` | ABI11、Copy位缓存；JSON短诊断及独立A/B |
| `599f36540783ee7223f50b0dd0e6bca1daf1ce8a` | 连接槽首次准入初始化、Java退出合同、native async边界修复 |
| `f7e984ae7827afe9b84053d76ba2a75ae94a27d5` | 有界Task整数范围及正式编排；七栈L1冻结源，49/49计算窗有效 |
| `9ce3c561fecaa9f50656f113bfc0694bff3937b2` | 仅既有C夹具／记录跟进；204项产品／基准输入与f7一致，有复用证明；必要三平台CI通过 |
| `8ad794036c7789bc0ae138c9f9360e40cf374887` | 删无消费者的progress读钟，字段布局保留；PR通过，push Windows实库失败 |
| `69f70d9ebbcddcca0cb02b152e3e2ca2228c9dd8` | 同步空错误邮箱及无期限inline入口；相关本机检查已补，收益待验 |
| `39c89756fb5d689cc633988c8c921b6eb4de3101` | 默认独立sync／async PG实库验收；两次CI均有实库失败，详见第7节 |
| `d15dfbbdfda91534f2ff38c1241c0041d566b186` | JSON parse／decode／try_parse只读调用及受控请求字符串投影 |
| `4ea1407e4c5f9690022f2fac1a8565c5f74ecf3f` | 当前产品HEAD：PG同一owner turn批量推进，保留共享预算；Linux已实编、三个独立短窗完成，配对收益未验 |

本机PG验收采用连接退休观察合同修正；该次构建时的dirty状态与源码／CLI哈希一并记录，不能仅用HEAD称为clean构建，更不能冒充三平台CI。Linux基准产物另由确切4ea归档构建。原主目录用户改动及本工作树预存的 `docs/multicore-capacity-acceptance.md` 修改不纳入本报告变更。

正式环境是M1宿主下Ubuntu24.04 ARM64虚拟机，服务／驱动／私有PG的CPU集与资源域分开。虚拟vCPU不冒充独占物理P核。Ku由CLI发射未修改C再用Clang18构建，仅是Linux ARM64研发产物，官方CLI该目标的native链接支持不能据此宣称完成。实际版本：Go1.27.2（ELF走encoding/json/v2）、Rust1.89.0／LLVM20.1.7、Clang18.1.3；JDK25.0.4.1+1、Vert.x5.2.2／Netty4.2.19／Jackson2.21.7；.NET SDK10.0.402／runtime10.0.13／Npgsql10.0.3；Drogon1.9.14／JsonCpp1.9.5。Node及完整依赖、锁文件、flags沿冻结manifest保留。

每个核心场景分别对Go和Rust验收：同SLO正确吞吐比95%区间下界均≥1；同速率CPU成本比上界均≤0.90；整服务稳态及观测峰值内存比上界均≤0.80；p99同时满足原SLO与默认1.05倍非劣界。计划正确率≥99.9%，错误正文／未知500／崩溃为0。7轮固定seed交错，每窗至少30秒；p99单窗≥100正确样本、核心公开结论合并≥1000个该类型样本，p999不足1000不报。混合保持70%JSON／20%PG／9%timer／1%CPU，各类型单独验SLO，不平均各轮p99。

V5采用有界对数直方图、互斥终态及全量CSV对账。窗口、驱动存活期和物理责任终结后的完整成本分别记录；额外5秒夹具静止门槛不等于原30秒请求／45秒handler期限，也不等于撤压后新请求5秒恢复。FIN可能是合法半关闭；后台或DB工作未物理结束前不退款／复用。不自动重放SQL，不把execution_unknown推断为回滚。

## 3. 成本证据与实际修改

| 路径／源码 | 已确认的额外成本 | 已实施处理／保留合同 | 仍缺证据 |
| --- | --- | --- | --- |
| Task计算：`c_task.rs`／`c_task_driver.rs`／`c_task_control.rs` | 257 CPU HTTP每请求平均528000迭代、33002次poll；459个软件样本中drive self52.07%、vDSO时钟self16.56% | 直接热状态、Copy值与初始化位缓存、phase地址缓存、有界整数证明；每原状态phase acquire、64状态配额、32poll／1ms保持 | 最新源完整HTTP有效A/B；drive包含业务与记账，不能全算调度；children不可与self相加 |
| 同步算术：`c.rs`／`c_sync.rs` | f7实际汇编中无期限仍有函数调用／TLS检查，空错误邮箱反复复制清零，checked整数有额外分支 | 69f小inline无期限入口＋原样慢体；空信号直接返回零成员，非空仍take/reset；4ea实际汇编已消除原无条件调用及空邮箱整块搬运 | 同期完整L1配对收益待验；不能删除可观察整数错误 |
| JSON：旧writer／parser、`metadata.rs`／`ir/mod.rs`／解释器 | b1时读取req.body有副本，取data再形成一次owned子树后move；Rust移出子树、Go共享引用；Ku并非两次整树复制，http.json仅编码一次 | 普通字符串／ASCII片段、int64整数writer；d15接通旧parser同步只读调用，受审查的intrinsic／用户View可读取请求字段原存储，结果仍owned；旧泛化stdlib View保持原快照 | 请求正文副本已定向消除，子树复制仍在；完整HTTP收益未测；暂停的通用复制／释放不展开 |
| 通知与网络：`c_http_runtime.c`／`c_task_reactor.c` | JSON kernel self约53%—57%、wake children约15%—18%；原net空集合两owner仍扫32768个job；4ea PG采样确认非空owner整容量扫描26/182 self样本 | HTTP活动表／free chain／代次映射，NET_WAIT owner计数空集合O(1)；本轮补每owner WAIT双链及slot/generation快照；POSIX非阻塞pipe＋pending，Windows原UDP | 最新活动索引实际PG／A/B待验；增加16384×8+32×4=131200B固定预算，不能称没有空间代价；syscall等仍待分解 |
| 固定存储／HTTP连接槽 | b1对象匿名内存差Rust约2.79MB，其中约2.52MB在预热前已存在；启动触写空槽约1.10MB | 保持calloc容量、稳定地址和全额预算，首次准入才初始化；复用及poll均核对high-water／owner／generation | 已测轻负载少驻留1.039MiB；剩余TLS／allocator／线程成本未全部归因，不能称泄漏或正式峰值达标 |
| PG：`c_pg_async_runtime.c`及原池 | 旧同步有线程往返和DISCARD；分段getter约0.93µs、acquire约0.57µs，不支持二者主导差距；原async每行反复owner往返 | query／result／reset接入NET_WAIT，connect仍可能后台；4ea让已就绪结果在原32结果上限内继续，校验＋复制共享64KiB／64 cells预算，实际扩容仍让出 | 原150ms下大结果超时的独立耗时归因、Windows复验、normal／reset-equivalent正式性能 |

固定512000迭代的初始精确计数为2048003逻辑状态、32001次drive；不同于上表平均528000输入的33002次poll。两者都保留原64状态边界。候选将完整frame检查544004→32004、动态selector2080003→32002、空scope退出512000→0，跨状态存活槽21→5、frame464→336B、Task实例960→832B；用户async helper的Task身份没有删除，公开直接格式化原语与原helper分表。

有界范围证明只检查纯Copy int/bool/null的 `(state,remaining≤64)`：同一操作的所有可达上下文都成立才省检查，每drive有int32入口guard、实际仍int64；范围外或一般／Owned／host／cleanup边界回旧路径。证明≤1000000 work，额外C≤256KiB；预算耗尽或挤占whole-file输出时关闭该优化完整重发原代码。保留增长平方、MIN/MAX、除零、MIN%-1、逐位置取消及spill反例。7单测＋24集成、相关ASan/UBSan5／TSan5通过，LSan未启用；小夹具C增7935B，编译成本与实际收益分开。

f7同步batch实际ELF排除了“每轮整数组复制”和“对照用了32位整数”：`ku_array_push_reuse_int`按容量增长；三栈64位标量，乘33均移位加，取余均无SDIV，此递推没有SIMD。Go／Rust在内部计时前预留结果容量，Ku增长在内部计时内；全进程CPU均计入，不能把源码容量序列当实测分配次数。

PG分段仪器本身使CPU比control增16.37%，不能当生产精确值。10400个含预热样本：排队wall28.57µs、worker CPU68.86／wall250.19µs、acquire CPU0.57／wall4.00µs、三getter CPU0.93µs、JSON CPU1.82µs、reset CPU22.84／wall83.01µs；send／wait／prepare／reset包含关系不能相加。normal与同lease DISCARD的reset-equivalent表分开；Go／Rust各2项清理／取消回归已过，不等于正式性能通过。

## 4. 表A—C：正式比较

### 表A：同SLO正确吞吐

| 场景／资源 | Ku | Go | Rust | Node | Java | .NET | C++ | Ku/Go速度95%区间 | Ku/Rust速度95%区间 | 状态 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| HTTP小响应，4核1GiB | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 | 未验收 |
| JSON对象，4核1GiB | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 | 未验收 |
| JSON数组，4核1GiB | 待验 | 待验 | 待验 | 待验 | 未纳入 | 未纳入 | 未纳入 | 待验 | 待验 | 未验收 |
| JSON转义，4核1GiB | 待验 | 待验 | 待验 | 待验 | 未纳入 | 未纳入 | 未纳入 | 待验 | 待验 | 未验收 |
| CPU HTTP，4核1GiB | 待验 | 待验 | 待验 | 待验 | 未纳入 | 未纳入 | 未纳入 | 待验 | 待验 | 未验收 |
| PG normal，4核1GiB | 待验 | 待验 | 待验 | 待验 | 未纳入单项 | 未纳入单项 | 未纳入单项 | 待验 | 待验 | 未验收 |
| 混合normal，4核1GiB | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 | 未验收 |
| PG／混合reset-equivalent，各4核1GiB独立表 | 待验 | 待验 | 待验 | 未纳入 | 未纳入 | 未纳入 | 未纳入 | 待验 | 待验 | 未验收 |
| HTTP／JSON对象／CPU，各1核1GiB独立表 | 待验 | 待验 | 待验 | 待验 | 未纳入 | 未纳入 | 未纳入 | 待验 | 待验 | 未验收 |
| CPU／JSON对象热点，各2核1GiB独立表 | 待验 | 待验 | 待验 | 待验 | 未纳入 | 未纳入 | 未纳入 | 待验 | 待验 | 未验收 |
| JSON对象，4核256MiB | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 | 未验收 |

容量边界尚未可靠确定，不能填最高短测点或将不同驱动／版本窗口拼成容量。JSON三语料、PG两种合同及不同核数最终分别出行；“未纳入”不是0分。

### 表B：同正确吞吐的资源与体验

每个实际场景／共同速率分别填以下各栈行；当前尚无最新候选正式成组结果，第1节短诊断不冒填。混合各类型保留独立p99、样本数与SLO。

| 场景／共同速率 | 栈 | 正确rps | p99／样本数 | 失败率 | CPU ms／正确 | 稳态MiB | 峰值MiB | 全生命周期CPU秒 | 线程／FD |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 待正式测量 | Ku | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 |
| 同场景 | Go | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 |
| 同场景 | Rust | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 |
| 同场景 | Node | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 |
| 纳入的同场景 | Java／.NET／C++各一行 | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 | 待验 |

每组同时列Ku/Go和Ku/Rust的CPU／稳态／峰值／p99比值与配对区间。Task分配账本、进程域内存、RSS／PSS和VA预留不是同一指标；全域分配／复制／syscall计数缺失写未测，不能写0或以“无GC”作省资源证明。

### 表C1：七栈独立同步计算，f7冻结源

同一1核1GiB域、32768次×512000迭代、seed3，每项checksum均22944003；7轮按seed20261023交错，共49/49有效计算窗，各进程wall及内部compute均≥30秒。全进程组绝对期限600秒，单流1MiB、100ms资源采样≤6016条，保留OS峰值；没有提高产品／HTTP期限或睡眠凑时长。repeat0冷启动另测，不相减；它是普通页缓存的新进程启动，不是冷盘。所有轮次保留，含第6轮Rust／.NET变慢的窗口。

| 栈 | 每批计算数 | 批量wall秒中位 | 完整CPU ms／计算中位 | fresh repeat0 wall ms中位 | 有效计算窗 |
| --- | ---: | ---: | ---: | ---: | ---: |
| Ku | 32768 | 72.175698 | 2.191016 | 2.287 | 7/7 |
| Go | 32768 | 75.989022 | 2.298799 | 4.578 | 7/7 |
| Rust | 32768 | 66.604358 | 2.006278 | 1.955 | 7/7 |
| Node | 32768 | 258.276010 | 7.869780 | 33.623 | 7/7 |
| Java | 32768 | 77.847996 | 2.353992 | 688.037 | 7/7 |
| .NET | 32768 | 71.778034 | 2.180079 | 358.653 | 7/7 |
| C++ | 32768 | 65.175000 | 1.969658 | 19.163 | 7/7 |

| 必验主对照 | Ku固定工作速度比95%区间（对照wall/Ku wall） | Ku CPU成本比95%区间 | 目标判断 |
| --- | --- | --- | --- |
| Go | 1.052587 [1.042989,1.060838] | 0.951783 [0.944136,0.961433] | 速度下界≥1；CPU上界未≤0.90 |
| Rust | 0.946893 [0.911149,1.008926] | 1.066449 [1.005567,1.104857] | 速度下界未≥1，CPU亦未达标 |

区间为每轮比值的几何均值、paired-window percentile bootstrap，seed20261023、20000次；不是把32768个结果当独立实验。第6轮Rust CPU81.245395秒、此前约64.45—66.34秒，.NET同轮77.855204秒、此前约69.75—72.42秒；没有按后验离群规则删掉，跨轮漂移仍可能影响区间覆盖。该表是f7普通构建有效结果，不是最新4ea收益；代码大小／调优构建另列待补，不以此表证明全部L1要求或整个专项完成。

### 表C2—C3：真实等待、持续负载与过载

| 项目 | 共同配置／栈 | 必要结果 | 状态 |
| --- | --- | --- | --- |
| timer／受控TCP分别等待 | Ku／Go／Rust／Node，1／4核1GiB，各32／128／512／2048／8192，真实持有30秒 | 实际同时等待数、完整返回、总内存／每等待增量、空闲CPU、普通探针、公平／恢复／自然退出待验 | 未验收 |
| 持续混合 | 七栈，4核1GiB，各600秒，同一可承担速率，70/20/9/1 | 按类型正确性／p99、稳态／峰值、任务／连接／队列字节趋势及回收待验 | 未验收 |
| 真过载／恢复 | 原四栈必验，新增三栈混合补验，4核1GiB | 真实饱和、各终态、同绝对压力与各自容量1.5／2倍（必要5倍）、撤压后新请求≤5秒、完整收尾成本待验 | 未验收 |

公开上限已接入16384 active／32768 connections及16384 Task／network，基准经普通config.json配置并核对管理limits。本机小组通过不等于Linux8192通过。CLI普通根默认1024、Task预算默认16MiB；正式等待使用64MiB Task账本且整进程1GiB域，另计HTTP缓冲、系统栈与外部库。8320 active／16640 connections、2owner／16384 Tasks的Mac固定backing35473944B，不是整进程峰值。

## 5. 表D：Ku优化前后

| 变更／工作包 | 对照身份与实际证据 | CPU／速度／内存变化 | 正确性、尾延迟与决定 |
| --- | --- | --- | --- |
| Task热状态／存活槽 O02 | 旧→1b；精确计数、C／ASM及73相关回归；Mac7对 | Mac完整CPU均值比0.880 [0.848,0.926]；frame／实例尺寸见第3节 | Linux正式收益待验；原状态／错误／取消与用户Task保留 |
| Copy局部值 O02 | 同ABI11同frame，仅替换drive；Mac7对128批 | CPU比均值0.90394 [0.89409,0.91464]，CPU中位0.579898→0.525739秒，maxRSS同1785856B | 原8批不确定点0.925 [0.867,1.017]保留；不是Linux正式结论 |
| Copy初始化位 O02 | 257→b1，Linux2核120rps，7对10秒，各1200/1200正确 | CPU比0.972507 [0.968957,0.976101]；稳态内存1.01953、峰值1.01073，无节省证据 | p99比1.04595 [0.99189,1.15457]，一次10.2ms，非劣证据不足；全部自然退出 |
| checked add/sub、phase地址与范围证明 O02 | 425／838／6b→f7；内建adds/subs与便携回退，实际ASM、相关回归 | 静态指令3938→3684的独立phase变体仍14个ldar；范围证明实际收益未测 | 算术第4旧窗、phase与range旧control均失败，候选不能据此判快慢；详见失败表 |
| 直接格式化／http.json O03/O05 | 公开API，direct／response／async／int各有输入身份 | 无正式完整调用收益 | 不删除用户async helper；http.json仅编码一次，相关API／资源／HTTP回归通过 |
| 活动槽／空net／pipe O04 | 原→1b／838及本轮owner WAIT索引；37+1及后续41 HTTP回归，满pipe、交错清除、4producer／1000唤醒 | HTTP活动对象与空net O(1)已有实现；最新非空net从整capacity改为本owner WAIT链快照，通知与索引实际A/B待测 | 原ASan/UBSan／TSan相关通过；本轮source22、PG11及Mac配置ASan/UBSan TCP2+PG11过，LSan关闭；初UDP合并方案弃用，Windows不合并 |
| 槽首次准入初始化 O06 | 204→599，C除HTTP runtime逐字相同；Linux7对14代70/70正确 | 启动／少量请求后匿名驻留均少1089536B≈1.039MiB；启动比0.623889 [0.623762,0.624141]，后续0.646869 [0.646746,0.647115] | 21线程／12FD同，账本归基线／自然退出；是轻负载驻留，不是正式峰值／Rust80%目标 |
| 旧JSON字符串writer O07 | 相同C仅writer不同，Mac7对×10000次，19边界回归 | 仅encode CPU比：对象0.619、数组1.006、转义0.558；完整链0.824／0.997／0.699 | 数组无明确改善；非HTTP结果；保留旧库、整数／浮点1e20／NUL／Unicode／深度／OOM规则 |
| int64 writer／ASCII parser O07 | b1→838，分离变体；20字节整数缓冲、普通ASCII片段 | 实际完整HTTP A/B待验 | 不改旧JSON默认、错误或owned结果，相关边界回归通过 |
| JSON请求正文只读 O07 | 39→d15，metadata／IR／解释器及4类借用回归 | 消除受控调用的请求正文副本，时间／内存收益待验 | 只读输入不消费，结果独立owned；泛化旧View不暗改，子树owned复制未删 |
| PG getter与owner query/reset O08 | 257同步→838；有限25/25、RST、GUC reset、Go／Rust同lease清理 | getter去字符串/JSON树；正式normal／reset表待验 | 物理job结束才退款，unknown不推断回滚；sync／async最新本机实库均过，三平台待验 |
| PG同turn批量结果 O08 | 39→4ea；每行额外往返由C证实，4项实际C回归 | 在原byte／cell／32结果边界内连续推进，收益待Linux／Windows实测 | 实际growth仍让出；共享校验／复制预算，不延150ms产品期限 |
| unused progress时钟 O01/O02 | 9ce→8ad，全产品无读取者，字段布局保留 | 通常完整32poll段driver读钟35→34，未测收益 | 实际出队／公平／期限／重排队时钟保留，旧私有诊断证据不改写 |
| 同步空邮箱／无期限helper O02 | 8ad→69f，包含于4ea；首错／清理／空成员相关检查 | 实际完整调用收益待验，不能由少调用推断比例 | 无期限0读钟，非空take/reset、真实期限与清理慢体保持 |
| O2／O3／LTO／PGO O09 | 旧1b源18实编、14黄金、4栈训练／留出闭环 | 无最终源正式性能结论，未改变默认flags | 插桩不入主表；最终源、flags、训练留出与各栈同级调优机会待验 |

轻负载高水位实验按各自RID核验原对象语料，不宣称请求URL逐字相同；PSS启动／后续比0.784721／0.792451，RSS比0.883988／0.886269，完整原轮次保留。active1024的旧基准fixed_bytes2858120、reserved1562864；小夹具104624B scratch不等于服务总内存，owner栈pending≤264B与内核pipe也不冒称Task堆。

## 6. 工作包状态与下一动作

| 工作包 | 已实现／验证 | 尚缺的有效测量或交付 | 目标状态 |
| --- | --- | --- | --- |
| F01 500原因／终态 | 8个有界受保护样本、RID前48字节hex，优先500；根未启动InlineFailed准入及独立拒绝计数修正 | 最终源原CPU触发点的全原因与物理退休；不将139旧LIMIT追溯归成纯槽不足 | 未验收 |
| F02 完整生命周期 | FIN／RST区分、唯一完成、CLOSED_JOB／ASYNC等物理退休；失败尾部CPU保留 | 最终取消／超时／断开／后台对账与可归因无用CPU；新请求恢复独立验 | 未验收 |
| F03 容量／过载 | V5／控制服务／有界编排；11680/11680控制请求正确 | 核心可靠容量低／中／临界／越界、真饱和及恢复；非单调旧失败未唯一解释 | 未验收 |
| F04 样本／身份 | 7轮／30秒、尾样本门槛与混合比例；f7 L1 49窗有效 | 最新源各核心点／原轮次／失败与区间；产物不能按版本号或改名冒充同源 | 未验收 |
| O01 成本分解 | CPU／JSON／PG／基础驻留及实际C／ASM；软件perf | 硬件计数不可用，全域alloc／复制字节／syscall计数未齐；按热点补缺口 | 部分完成 |
| O02 C计算热路径 | 直接状态、Copy缓存、有限范围、同步快路径；相关回归 | 最新Linux实际C／ASM与有效CPU HTTP A/B、公平性及正式目标点 | 未验收 |
| O03 helper／布局 | 公开直接原语、frame收紧，不删除可观察Task | 原helper与正常API分别比较完整成本 | 未验收 |
| O04 owner／通知 | 活动索引、代次、空集合计数、POSIX pipe合并及并发回归 | 同速率CPU／空闲CPU／扩核／等待与公平 | 未验收 |
| O05 HTTP | 活动路由／已有gather、json MIME与唯一响应责任 | 同语义完整吞吐、写回syscall／复制、partial-write／断开及资源 | 未验收 |
| O06 资源 | 容量／固定账本、lazy触页；轻负载少1.039MiB | 同速率整服务稳态／峰值、8192等待与收尾保留量 | 未验收 |
| O07 JSON | 旧默认、writer／parser局部处理、只读请求正文及边界回归 | 三语料组件A/B、最新完整调用／资源，类型化与动态分表 | 未验收 |
| O08 PG | 原共享池owner状态机、同turn预算；本机sync／async均过 | 原Windows大结果150ms失败相关验证、最终三平台实库、normal／reset正式对照 | 未验收 |
| O09 调优 | 旧源实编及训练留出闭环 | 最终源普通／调优有效表、flags／大小／编译时间及产物身份 | 未验收 |

## 7. 正确性、CI与历史失败

本轮owner WAIT索引补完后，现有source target22项与PG poll target11项通过，1个原有live opt-in未启用；独立只读复核未发现正常调用下的具体错误。严格Clippy（all-targets/all-features、-D warnings）、全workspace格式与debug CLI构建通过。Mac配置ASan/UBSan的TCP两项及PG11项通过，`detect_leaks=0`，不冒充Linux强制检测器。实际现有私库脚本再跑sync／async，两路均完成10000行、中文／NULL、原150ms超时、断连、确认丢失恰1行及原5秒恢复；exit0、远端连接0、cleanup为空，async unknown7／confirmed_cancel0／未确认回滚。该次CLI `40c2e42c1f8e86c4ce38896bc3a07f3af59adc09f7303994e6dc71650cbf37cc`，报告SHA `79d0c78d89b61659910fba6a7e929e46cdccae660ba2ab27262fee74594bb9f8`，是在86f基础加本轮dirty生产改动构建，不按HEAD冒称clean。

最新本机实库使用独立sync／async私库／代理、debug native C、4 event loops、4池连接／8 waiter；原acquire100ms／query150ms／handler2000ms、180秒外层及故障恢复5秒不变。两lane均passed，10000行2列、Unicode／NULL、超时、新读恢复、断连、写确认丢失均有实际结果；丢失确认的写恰1行，原SQL未重放。两次自然退出0、cleanup_errors均空、退出连接最终0。本机PostgreSQL17.11结果不能替代Linux16或三平台CI。

`pg_stat_activity`明确：async压力join后首次观察有5个远端session，其中4个仍在PgSleep、1个idle；原5秒窗口内观测为约0.207秒5个→0.739秒1个，非持续泄漏证明。压力观察与join共用原5秒，drain／进程退出／远端归零共用从drain请求开始的5秒，最多8次有界查询；不再要求close之后瞬间远端归零，也不延长窗口或确认回滚。该验收修改尚未提交，前一失败 `pg-async-connection-diagnostic.json` 保留。

同一批源码本机定向native／借用／同步回归49通过、1个原live opt-in ignored；strict all-target Clippy与fmt通过；借用及PG实际C ASan/UBSan合计6项、TSan4项通过。Mac `detect_leaks=0`，不称LSan或全Runtime竞态审计通过。首次分配夹具因旧string.trim native不支持失败，改用已有string.replace保持快照对照后通过；原失败日志保留。receipt记录HEAD39c＋当时dirty源码哈希，产品修改随后提交d15／4ea，不能仅凭HEAD把此验证写成clean4ea完整CI。

| 身份／失败 | 实际观察与处理 | 当前边界 |
| --- | --- | --- |
| 1b CPU高压，原helper | 4800计划、223正确、3389个503、1188超时；客户端无500，内部139个500均task_limit，unknown500=0；子Task失败Result沿原链传播，非根未启动准入 | LIMIT旧码兼槽／字节，不能唯一判槽不足；完整CPU37.072111秒、驱动后21.804045秒，5秒夹具未过，原期限内自然退休 |
| 257 direct／response／int高压 | 4800计划、275正确，内部500／unknown均0，3361个实际queue_admission拒绝，槽／字节拒绝0 | 驱动后CPU19.520660秒、10.139秒才静默，5秒夹具未过；不把旧helper139次错误追溯写成已修，不等于新请求恢复已过 |
| 算术单变量425，第4旧control | 1132/1200正确、68超时即停止；前三对完整正确，第四新候选未运行；unknown500=0、自然退休 | 保留全部窗口，不剔除失败重算有利区间；同期节流／驱动延迟不足以唯一归因 |
| phase 30秒v1／v2 | v1编排重复查询已冻结旧服务，旧3600正确、新未运行；v2旧1656/3600正确、1944读响应头超时，新未运行 | v2全部3600计算／响应构造完成，fullCPU65.040784秒、post5.471166秒，约3秒后物理归零；不能作候选收益 |
| range 599→f7，30秒v1 | 首旧control2903/3600正确、697读头3秒超时，status0／响应字节0；新候选未开始，停止并保留 | 完整CPU33.526652秒、post2.509014秒；3600计算与33002poll/计划均完成，正常exit0且5秒回收过；服务与驱动同期采样空档增CPU，未唯一定因，不能将697说成服务504 |
| 204冷态mixed黄金 | 200计划、181正确、19个DB acquire_timeout；后续有限冷态／同条件未复现 | 原失败保留，未定因或声称修复；client_open实际listen前建首连接，不是空池 |
| 986早期detector | scheduler probe仍硬认ABI10，新ABI11首断言停止；更新旧前提后4项过 | 不是内存探测器发现错误；相应旧失败保留 |
| b1 CI | PR39975648033必要项过；push39975641602 Windows恢复8次读在0.984秒耗尽、Mac上传DNS ENOTFOUND | 恢复观察改覆盖原5秒且最多8次，不延产品期限；上传失败不写成编译失败 |
| 9ce CI | push40006709264、PR40006719621必要项通过，push三平台包与asset-set成功；PR发布任务预期跳过 | 只覆盖该SHA／实际复用范围，不替代最新源码；测试C入口／Windows libpq目录夹具修正未改产品 |
| 8ad CI | PR40010779946过，push40010773305 Windows慢查询unknown后首个新读acquire_timeout约110ms，随后JSONDecodeError | 验收复用原5秒／8次新只读合同，原查询／INSERT不重放；pending0和reserved上限不能证明idle／leased |
| 39c CI四种新失败 | push Linux／Mac async压力远端5>4；PR Windows drain exit0后瞬时远端1≠0；push Windows async10000行172ms超原150ms返回unknown | 本机远端观察补证据、验收改共享有界退休窗口；4ea批量结果处理待平台实测，不称四类根因均已修 |
| 86f push40024100907 | 三平台PG的sync／async步骤均success；Windows Caddy rolling资源快照spare41线程>33，cleanup采样PID1956仍存在 | spare为supervisor6＋launcher6＋两个直接子12／17线程；Toolhelp旧记录缺映像／创建时间，不能唯一判断两个Ku实例重叠或PID归属问题，不增加reserve掩盖；失败与PG性能分开 |

39c PR40015062381／push40015050574均failure；Rust workspace步骤、Clippy及已有detector通过，三平台包构建／包内小范围smoke有成功回执，但不能写成实库全矩阵通过。现有包smoke覆盖解包／安装CLI版本、hello、JSON、HTTP两loop的/ok／cpu／timer及正常drain；不含安装包PG运行。Actions外ZIP digest不同于内归档hash，完整39c包未在本机下载重算。最终4ea及后续必要CI／同源安装包仍待验；新增sanitizer workflow因OAuth无workflow权限尚不能上传，不绕过权限，普通Git推送另行执行。

另一个有序退出修正限Java基准：同4.5秒绝对预算依次HTTP drain／在途0／CPU executor／PG pool／Vertx，严格匹配实际SIGTERM、PID／listeners、阶段／计数、无OOM／强制终止和cgroup清理才接受143；其他栈要求exit0。19项驱动回归及真实PG／timer在途退出通过，不把任意非零退出放宽。

唯一后续599有界8秒诊断960/960正确、最大计划到完成29.74ms，未捕获所选异常；全局环覆盖1196条早期记录，不称trace整体完整。collector错用 `/proc/TID/stat` 的线程user/system拆分已剔除，仅独立schedstat可用于运行／就绪等待；未借此归因平台。诊断开销不入正式比值，服务／驱动／perf正常回收，临时C／ELF／脚本已清理。

历史最早3个504仍是“证据不足，无法唯一还原”；后续历史7次及旧正式30分钟未通过仍保留其版本／夹具范围。当前短测与不同路径的通过不改写旧根因，不追加盲目历史长压测。

## 8. 证据索引、缺口与交付边界

证据存项目外 `~/Downloads/Ku_性能对齐_2026-10-23/`，不是安装包内容。宿主目录及任务书曾暂时不在原位置；用户告知恢复后已核对目录及任务书存在，PG双lane、相关回归与range失败analysis三个原件SHA均与先前记录完全一致。Linux虚拟机 `/opt/ku-bench` 的冻结源和原始测量也保留，两个本轮PG采样归档已传回宿主并逐项核对大小／SHA。原入口：`profiles-analysis/{cpu-analysis.json,task-range/}`、`compiler/copy-init-linux-summary.json`、`json/`、`pg/`、`pg-owner-cost/`、`fixed-backing-readonly/highwater-residency/`、`l1-f7-formal/`、`phase-pointer-linux-30s-v2/analysis/`、`range-599-to-f7-30s-v1/`、`ci-{8ad7940,39c8975,86f5948}/`、`current-validation/`。失败证据不因新通过删除；公开报告不得包含私库URI／凭证。

| 证据 | SHA-256／适用范围 |
| --- | --- |
| f7完整batch原summary | `97598cf52ff36c05ac798184ee21d331fbbe470a6563dc275582437b44afeb9c`，40097696B原件，非compact文件hash |
| f7 summary-compact／配对比较 | `daa610a5a5b88d2fdfdfbce31ffeef254cb7d0460cab87ec9b87f18a7cda324b`／`81cb208435e6efdce642fd1f0a25634de291fe39250c364813173dc139ca021b` |
| f7冻结manifest／协议 | `b4d7d1424633be07df616d74f96eae346462b7bd87d43b267234b6ac2ee9cbcf`／`2057816794f2907f1aee6e0139a734fedbe5be20475c32c86a59df1f63c24fd1` |
| range旧control失败analysis | `c9d2e0a1011b31dfebc74b9cde94e4e62099247577543d51402132fa64de54ac`；raw9原件588091B逐项校验，3600CSV行无丢弃 |
| 本机PG双lane最终报告 | `763880f2bf8484d1fd1e1aa6c1e8d95a1317d49ae48170d9bee1cdaf35413159`，debug CLI `9166c2f3e644d3261c7999f8b13c330ae8004ce2d71aaabb2e85b3e0adcdbd6b` |
| 本机相关回归／detector receipt | `5299c1db797e258e463f03342cc6b7c7ec0f31d1202cd958dfde1922f6c26731`，含dirty源hash、原失败与Mac LSan限制 |
| 4ea PG预热首次失败 | guest `profile-pg-4ea1407-v1.evidence.tar.gz`，28254B，`d33b8f8245699d09033de0d1b52ca26dca96b9259f8d8e01245a6fde6f5141d5`；无perf样本，不能用于热点比例 |
| 4ea PG冷态有界热点诊断 | guest `profile-pg-4ea1407-cold-v2.evidence.tar.gz`，589464B，`8c5b00af34d2f3a2dcb6e92e823cc7e4e18546f985a54c5a13fc41c93b931d13`；182软件样本，含原perf／实际ASM／全部错误与回收记录 |
| 8ad Windows失败外ZIP | `72e3c4caeae6d32aa4d73f2623f7ca2e89ad546ee10675a22aa7558975da7429`，API digest与下载匹配；不是release内包hash |

尚需连续完成：最新候选实际Linux C／ASM成本闭环及有效单变量收益；不依赖失效窗口的正式A／B容量和同速率核心点；normal／reset、8192等待、600秒混合、真过载／恢复；最终三平台必要CI及同源CLI／native／实际安装包PG适用示例和hash验证。结果未跨门槛保持未验收，不能以测试数、包构建或其中一栈较快代替目标。

JSON保持旧默认；暂停的通用值复制／释放后续分析与广泛AST分析不展开；无关36次进程内存验证不重跑。本轮没有部署官网、修改旧r1标签／资产、合并或公开Release，未操作真实业务库。保留用户dirty、原源码／测试／历史证据；自己的临时探针、脚本、构建副本及进程用完清理，必要永久回归／基准保留。
