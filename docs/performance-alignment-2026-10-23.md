# Ku 性能与资源对齐：实施及验收记录

## 2026-10-24：同步 HTTP 正文所有者转移

同步 arity=1 处理器原先把已接收的 `input.body` 再分配并复制到 `req.body`。现在在投影准入成功后直接转交同一 owned 缓冲，只清输入指针；未命中路由、准入失败、未收齐正文和异步处理器保持原路径。对 N 字节非空正文，少一次 N 字节分配及 memcpy，不能外推整体吞吐收益。

保留原投影中的正文预留和 `input_drop → response_admit → projection_refund` 顺序：输入退款后，投影继续覆盖转交的缓冲；响应准入后才退投影。预算仍保守预留，没有宣称减少该账本额度。原生成器负责请求及返回值的 move/drop，不在 C 调用点再次释放请求。现有 HTTP 回归继续覆盖正文返回、局部捕获、错误／期限、部分读取／写回和配额恢复；增加生产输入生命周期的地址及恰好一次释放反例。

另修永久跨栈执行器的旧前提：JSON 已改为显式 `take`，`test_compile_variants.py` 仍要求旧索引语句，实际旧门禁失败。本次核对解码、counter 读取、一次 take 的顺序及转换范围，不通过删除断言放行。正式性能、七栈及8192目标仍未验收。

## 2026-10-24：Windows 验证程序进程归属修复

`c19506e` 的PR必要12作业通过；push为18过、Windows Caddy作业1失败，旧失败保留。该机编译器进程2528被残留PPID误挂到新native 8904，误加15线程，将slot实际24计为39而超过33；cleanup再把同一无关PID报为残留。本机受控反例复现39!=24。没有提高限额或结束无关编译器，也没有据此修改Ku运行时。

定向调整`deploy/service/verify.py`及现有回归：按进程实例核对父子边；资源采样期间身份或树变化、查询失败均不作为通过；清理区分同实例残留、PID复用和未确认。UTC创建时间倒序时，另以同句柄的中断创建计数复核；故障注入始终用该次序验证，并在持有的目标句柄上核对实例后终止。查询采用固定64KiB上限，未知ABI/长度/身份均失败，不读取变长命令行。33线程、总预算、恢复期限和产品执行路径保持。

Windows进程句柄与PID生命周期见[官方说明](https://learn.microsoft.com/en-us/windows/win32/procthread/process-handles-and-identifiers)；UTC可调整，中断计数次序依据[进程元数据结构](https://learn.microsoft.com/en-us/windows/win32/devnotes/process_telemetry_id_information_type)和[动态查询接口class64](https://learn.microsoft.com/en-us/windows/win32/api/winternl/nf-winternl-ntqueryinformationprocess)。接口不支持时明确未验证，不能静默漏计。该修复是必要验收工具的归属纠正，不代表性能目标达成。

## 2026-10-24：定向修复，性能目标仍未验收

默认实现本批保留：方法 receiver 只检查一次（旧代码把 `Make(text).len()` 等嵌套调用误判为二次 move）；JSON 的显式 `KuValue.take(key)`；小对象编码排序 scratch；PG 取得池锁后重核原获取期限；有界失败现场及 HTTP Task 身份关联；互斥 blocking-job Result 联合存储；未注册 async HTTP handler 不再引用未生成的 thunk。OOM与获取期限同时发生时，诊断遵循原OOM优先级，不再误记为超时。没有增加期限、重试 SQL、替换 JSON 底层、修改通用 Value／AST 暂停项。

`take(key)` 消费 JSON-compatible 字符串键对象的 owner，返回独立拥有的 `KuValue!`：例如 `data = decoded.take("data")?`，随后不能再用 `decoded`。它按原约定报告类型／缺键错误；普通 `decoded["data"]?` 仍复制成 owned 值。解释器与 native C 同步路径有回归；解释器跨 await 结果仍存活，native Task 动态 KuValue 局部存储仍不支持。旧 struct／object 的 callable `take` 字段不被占用。

PG 失败样本最多 8 条，每条最多 8 个槽与 8 个等待者，缺失、竞争丢样、覆盖和部分导出均显式报告。既有受控 metrics 通道将 hex 编码的请求 ID 与 Task slot/generation 关联。私有实库已确认：单连接被 `pg_sleep` 持有时，第二个请求在获取期限到达且未发送 SQL 的阶段失败，随后请求成功，服务自然退出、会话归零。此受控容量反例不解释历史 mixed acquire_timeout，也不解释历史 7 次 HTTP 504；后者继续保持“历史出现过、本轮短测未复现、根因未确认、正式 30 分钟验收尚未通过”。诊断还缺同步线程栈／CPU及驱动环境现场，不能声称每次异常现场完整。

macOS ARM64 的 network-only Job 从 1072B 降为 720B，未减少连接／作业容量。PG 新增诊断固定 20985B、每池 `16+16*连接数` B，以及每个异步 operation 的完整新增176B（含112B lease现场）也计入开销；局部布局不是整服务内存目标的证明。

CPU 批量入口实验 `249576c` 及同步邮箱事实传播实验 `d25060d` 均退出默认，保留历史和独立实验分支。前者降低 C 交接次数，但汇编出现热变量栈访问，短窗没有有效收益；后者虽令单计算 C 层 take 入口 2560002→512002，clang 原本已合并读取，46 条正常热循环机器指令完全相同，不能把源码操作数当运行时收益。已有空邮箱和无期限 inline 快路径继续保留。

本轮七栈共同业务、CPU≤0.90、稳态／峰值内存≤0.80、8192 真实 timer／TCP、过载恢复和正式持续窗口均未验收。宿主 8GiB 短配对仍有换页，失效窗口不计排名；没有独立 Linux 机器，先完成其余本地工作。不复用旧包作为本批产物，不创建发布标签或改官网。

对应《Ku_Go_Rust性能对齐与资源领先_完整实施任务书_2026-10-23.md》。本轮仍在实施：已取得一组有效的七栈单核计算结果，尚未完成全部业务、容量及资源目标。**实现完成、测量有效、目标达成、安装交付分别判定。** 本文按结论与证据组织，历史失败不覆盖，未测项不填推测值。

已验证交付基线为`f833ae508770ef6ad42bfd2c41de67a1db088f2e`：[push40052428315](https://github.com/fengyanweb/ku/actions/runs/40052428315)的19个必要作业、[PR40052433089](https://github.com/fengyanweb/ku/actions/runs/40052433089)的12个必要作业通过。push三平台实际安装包、资产集合及包内CLI的sync／async PG共6路均实际执行成功；发布步骤按分支规则跳过，没有公开Release。该基线修复准备窗异常仍放行及工具树未确认退出时的lane隔离；此前3b／d3失败保留，不能据新通过改写其未知原因。CI只覆盖该提交，不覆盖之后的本地产品修改。

v4等待组已结束，8192目标未通过；v5已按独立业务源／执行器身份冻结，但未启动负载。用户确认没有独立Linux测试机，本轮先完成其余本地工作，不继续在当前换页条件下追加正式压测；完整HTTP性能和资源目标仍未验收。

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

e1ca实际HTTP采用direct格式化，仅一个root Task，旧三个格式化子Task没有执行。热迭代经过4个状态检查，约16个业务迭代一个poll；Go／Rust对照同步计算循环没有同等频率的应用级取消／让出检查。当前范围快路直接乘加取余，不能继续说每次运算都调用通用错误函数。同步batch没有Task，另有正常路径算术／错误／期限检查及寄存器布局成本；上述结构证据不是当前CPU占比，旧采样不移植为新结果。

f7固定计算对Go的配对速度比为1.052587、95%区间[1.042989,1.060838]；CPU成本比0.951783 [0.944136,0.961433]，未达到≤0.90。对Rust的速度比0.946893 [0.911149,1.008926]，不能证明追平；CPU成本比1.066449 [1.005567,1.104857]，同样未达标。最新JSON／PG及同步快路径改动不在这批冻结结果中。

最新c1cc单核正式七轮已完成：49个计算窗口、49个独立启动观察均通过，32768×512000迭代／窗、checksum22944003；所有进程exit0、98个资源域移除，无OOM／超时／清理错误。完整进程CPU／正确计算的中位数为Ku2.246322、Go2.434886、Rust2.046793、Node9.188046、Java2.365531、.NET2.239488、C++2.107894ms。Ku／Go配对CPU几何比0.912797 [0.862131,0.955457]，速度1.101869 [1.041212,1.188580]；Ku／Rust CPU1.091199 [1.047786,1.139591]，速度0.913488 [0.868744,0.957167]。沿用整轮配对、20000次bootstrap、seed20261023，Go round4等漂移全部保留。CPU上界≤0.90对两者均未达、Rust速度下界≥1未达。本批没有跨版本配对，不能将相对f7的变化归因于某项优化。

最新4ea已实编并完成三个独立短窗，均driver合格、自然回收、5秒夹具通过、cleanup为空：

| 4ea独立窗口，2核1GiB | 正确／计划 | 完整CPU／正确 | p99 | 范围 |
| --- | --- | --- | --- | --- |
| CPU HTTP，120rps×30秒 | 3600/3600 | 4.659532ms | 8.64ms | 单窗口，无同轮Go／Rust或旧源配对 |
| JSON对象，2000rps×8秒 | 16000/16000 | 65.4935µs | 1.70ms | 单窗口，非容量 |
| PG，2000rps×8秒 | 16000/16000 | 135.388875µs | 15.36ms | 单窗口；成本高于旧同步107.436µs诊断值，正在有界采样定位 |

该数据不能与257／b1跨时间、跨实现短窗相除当成单变量改善百分比，也不能冒填正式表A／B。原始目录为 `/opt/ku-bench/results/short-scenes-4ea1407-v1`；后续只补相关成本与有效对照。

PG后续第一次诊断在100rps×30秒预热中出现31个HTTP500（2969/3000正确），保留的24个响应体均为`database_error:acquire_timeout`，另外7个体未保留，不能补写其原因。尚未开始perf，因此没有可用栈；同期driver调度最大延迟570.8ms，不能唯一归因服务或宿主。保留该失败，不自动重跑或延长产品期限。

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
| `4ea1407e4c5f9690022f2fac1a8565c5f74ecf3f` | PG同一owner turn批量推进，保留共享预算；Linux已实编、三个独立短窗及热点采样完成，配对收益未验 |
| `b55e7ee62a13171fb070d172b54b09572771418b` | 当时产品源：每owner WAIT索引与代次快照；本机回归／私库通过，Linux按下行源实际重编；A/B首异常停止，无收益结论 |
| `c1cc3dc96a478cf799f9bb9815920e0a73108f06` | b55产品＋O09身份helper；Linux实际CLI／native构建170.715秒，manifest完整；batch C／ELF与4ea逐字相同，server已切换活跃链，原容量扫描消除 |
| `d450b3e3ada4a5c35741301df790bc4777785266` | 仅Windows验收身份记录及报告后续；src／基准输入／锁与c1cc相同。已推送，push40027977065／PR40027992019必要检查均成功；push三平台实际包及资产集合检查成功。分支发布步骤按原规则跳过，不算发布通过；包内CLI的PG链仍是独立缺口 |
| `e919a408` | 已复现的PG后继等待者通知修复；三个反例前失败后通过，不等于后续偶发acquire_timeout已经定因 |
| `8f42e2cf` → `e1ca2259dde09822bb85de9e015d060461c91695` | checkedmul候选短配对CPU回退，已定向恢复旧实现并保留边界回归；当前Linux性能产物来自e1ca，后续目录／门禁提交未改产品源码 |
| `f2b668d`／`d3b0c00`／`3b4e724719f33e445bdde516cf9b7214a731141c` | 安装包实库门禁、私有目录处理及重复单测清理；这些历史运行存在失败或跳过，原结果见第8节 |
| `f833ae508770ef6ad42bfd2c41de67a1db088f2e` | 已验证交付基线；准备窗及工具终止后lane隔离修复，push19／PR12必要作业及三平台实际安装包PG共6路通过，不代表完整性能目标通过 |

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

c1cc实际Clang18布局为job1200→1208B、pool2600→2728B，固定账本增量`128+8*capacity`；16384槽是131200B，本轮HTTP夹具实际2048槽则为16512B，不能混用。实际server ASM沿owner head／next构造快照，slot／generation及原位输出均存在。CPU frame2的C函数相同；整ELF函数仍3924指令／25 LDAR，但99条地址立即数导致170机器字节不同，不能称整函数字节相同。232字节核心计算块确实逐字相同，仍4 phase acquire与4预算检查。

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
| timer／受控TCP分别等待 | Ku／Go／Rust／Node，1／4核1GiB，各32／128／512／2048／8192，真实持有30秒 | v4四核40档已结束：13通过、8未通过、19前置阻塞；一核及8192目标、完整资源对照仍待验 | 未验收 |
| 持续混合 | 七栈，4核1GiB，各600秒，同一可承担速率，70/20/9/1 | 按类型正确性／p99、稳态／峰值、任务／连接／队列字节趋势及回收待验 | 未验收 |
| 真过载／恢复 | 原四栈必验，新增三栈混合补验，4核1GiB | 真实饱和、各终态、同绝对压力与各自容量1.5／2倍（必要5倍）、撤压后新请求≤5秒、完整收尾成本待验 | 未验收 |

公开上限已接入16384 active／32768 connections及16384 Task／network，基准经普通config.json配置并核对管理limits。本机小组通过不等于Linux8192通过。CLI普通根默认1024、Task预算默认16MiB；正式等待使用64MiB Task账本且整进程1GiB域，另计HTTP缓冲、系统栈与外部库。8320 active／16640 connections、2owner／16384 Tasks的Mac固定backing35473944B，不是整进程峰值。

v4四核本轮最高通过档（timer／TCP）为Ku32／32、Go128／32、Rust2048／128、Node32／32，仅代表已满足30秒保持及探针门槛的档位，不是容量极限或语言排名。Rust timer8192在预热阶段失败，目标组未启动；其余失败阶段及探针原样保留。21个实际执行服务均exit0、无强制终止且进程／线程／FD归零、cgroup移除；这不覆盖未通过的业务级TCP释放检查，例如Node TCP128。探针最多约56个样本，不据此发布p99／p999排名。主机换页与各异常尚无逐请求因果证据，不能将环境观察写成根因。

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
| PG getter与owner query/reset O08 | 257同步→838；有限25/25、RST、GUC reset、Go／Rust同lease清理 | getter去字符串/JSON树；正式normal／reset表待验 | 物理job结束才退款，unknown不推断回滚；本机及f833三平台实际安装包sync／async共6路实库通过，后续修改另验 |
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
| F04 样本／身份 | 7轮／30秒、尾样本门槛与混合比例；f7及c1cc各49个L1窗有效 | 最新源各核心点／原轮次／失败与区间；产物不能按版本号或改名冒充同源 | 未验收 |
| O01 成本分解 | CPU／JSON／PG／基础驻留及实际C／ASM；软件perf | 硬件计数不可用，全域alloc／复制字节／syscall计数未齐；按热点补缺口 | 部分完成 |
| O02 C计算热路径 | 直接状态、Copy缓存、有限范围、同步快路径；相关回归 | 最新Linux实际C／ASM与有效CPU HTTP A/B、公平性及正式目标点 | 未验收 |
| O03 helper／布局 | 公开直接原语、frame收紧，不删除可观察Task | 原helper与正常API分别比较完整成本 | 未验收 |
| O04 owner／通知 | 活动索引、代次、空集合计数、POSIX pipe合并及并发回归 | 同速率CPU／空闲CPU／扩核／等待与公平 | 未验收 |
| O05 HTTP | 活动路由／已有gather、json MIME与唯一响应责任 | 同语义完整吞吐、写回syscall／复制、partial-write／断开及资源 | 未验收 |
| O06 资源 | 容量／固定账本、lazy触页；轻负载少1.039MiB | 同速率整服务稳态／峰值、8192等待与收尾保留量 | 未验收 |
| O07 JSON | 旧默认、writer／parser局部处理、只读请求正文及边界回归 | 三语料组件A/B、最新完整调用／资源，类型化与动态分表 | 未验收 |
| O08 PG | 原共享池owner状态机、同turn预算；等待交接3个反例修复、14回归、本机实库及f833三平台实际安装包PG共6路通过 | 偶发acquire_timeout仍缺现场因果证据；normal／reset正式对照及f833之后修改的必要检查 | 未验收 |
| O09 调优 | e1ca实际13个新变体、3个完整训练／use、9个独立正确性留出；26个短观察原样保留 | Rust训练失败，2个use位置未执行；短窗非平稳且后13窗可能受外部交互影响，正式normal／tuned配对尚缺 | 未验收 |

### 独立机器缺失后的本地收口

本轮定向处理两处有源码依据的冗余。HTTP完整头首次验证同时累计无符号Content-Length，去掉从headers map查回并再次验证的步骤；纯数字溢出仍在原位置返回400，保留Expect、目标长度和metadata准入的错误优先级。只扩充原HTTP边界用例，不改公开headers、队列、超时或响应协议。无子Task函数则不再生成从未使用的receipt占位及零次循环；wait、timer、空scope期限／取消／清理保持，有子Task的布局保持。没有改Task的四次phase检查和清理状态写入，因为现有逐状态取消与清理合同依赖它们。

本机65项既有HTTP／Task回归、全workspace格式、all-targets／all-features严格Clippy和debug CLI构建通过。相同基准输入的8个真实编译实例中，7个无子Task实例各少32B：timer／waiting为1760→1728B，TCP为3632→3600B；有子Task的2712B实例不变，frame定义逐字相同。配置Mac ASan／UBSan的修改前后1740组头解释对照中，状态／正文长度／公开headers指纹／预算及回收均相同；Mac未启用LSan。以上是代码、尺寸与行为验证，没有测得端到端CPU、RSS或吞吐收益，不填入正式领先表，也不解释历史偶发超时。一次性C／可执行文件和调试副本用完删除，精简结果及源身份保留在项目外`current-validation/local-closeout/`。本轮生产改动仍须自己的提交CI，不能借用f833通过记录。

O09编排另修正一个已复现的身份丢失问题：重建Go／Rust变体后，原helper未保留PG模式及源码清单，`pg_variants.derive`正确拒绝该产物。本轮仅在实际输入与原完整源码清单一致、两个模式声明有效时继承这两项；ELF哈希仍取新产物，旧flags及产物只放normal参考。两模块17项单测通过，不把mock编译的单测当成实际调优构建。旧1b四栈业务源均已改变，旧调优产物／profile不能复用为最终源；旧batch训练输入又与普通L1相同，最终PGO需重新冻结代表性训练／留出，不能仅改标签称已验收。

checkedmul候选8f42已定向拒绝：compiler／portable各4211二元＋29取负、858实际同步边界及Task量子／取消／清理相关12回归通过；Mac与Linux均实际生成C／ASM。Linux旧乘法边界的4个MOV/MOVK消失，但取模魔数转成每轮4个MOV/MOVK，HTTP frame从15696增到17292B，fast region静态58对54指令、phase acquire与预算各4保持。在同一新boot、1核／1GiB、相同输入512000／seed3／repeat4096的两个短交错配对中，候选完整CPU比1.094985／1.095854，均正确／exit0／域回收。仅短R&D，不作正式区间；据此恢复旧c_int实现，不追加长测证明候选，保留扩展边界回归及8f42历史。该回退不撤销e919 PG等待交接修复。

后台线程驻留归因已实际执行：冻结c1cc同一ELF、2 owner／2核／1GiB，仅总后台配置3对17，7组交错14代均通过、自然退出且域回收。启动及少量/ok后实际线程7对21、FD同12；七组配对中位整域差659456／663552B，匿名差237568B、kernel_stack差229376B。配置差约0.63MiB不能解释此前全部Ku/Rust驻留差，也不构成产品优化或正式内存目标通过。JSON同步fallback可能用足原线程，尚未据此更改线程默认或容量。184个快照／原smaps／generation已核验归档；host／guest临时控制器按SHA删除。

数字解析的只读核对排除了另一种误归因：当前native/C整数直接从原字节累加uint64幅值并检查int64边界，没有逐整数临时字符串、strtoll／strtod或malloc。只有浮点token使用临时缓冲与strtod；三份语料分别34／257／1个整数、均0浮点。解释器的chars Vec与数字String不在本轮native链中。旧b1 array profile的parse_value self仅16/598，且包含所有类型构造，不是数字解析独立热点；因此没有据此继续修改数字解析。详见项目外 `evidence/json-numeric-cost-review/` 的源码身份与原profile索引。

## 7. 正确性、CI与历史失败

本次另确定一个独立的PG异步等待交接缺陷。A已被通知但未运行时，B为保持先后顺序而排队；A超时／取消后，原unlink没有将仍可用的idle连接通知给B。双连接池中，A成功取得一个连接、另一个仍idle时也能触发，所以修复不能仅限slot<0分支。最小修改位于`ku_pg_async_unlink_locked`：实际等待成员退出后复用原handoff helper；原helper对回调持有pin并在pool锁外唤醒。连接、查询、期限、SQL及自动重试规则不变。

三个永久反例在修复前全部失败于B没有readiness，修复后整个PG target14项通过、原live opt-in1项未启用；配置Mac ASan/UBSan同14项、TSan关闭／回调交错1项、strict Clippy和debug CLI构建通过。该CLI SHA `4f60900361270705a41038584827d7fe7d2aa4743f0e920793595967b5be136d` 在d450＋本次PG修改构建，尚非clean提交产物。真实PG17.11 sync／async两lane均过：10000行、Unicode／NULL、原100ms acquire／150ms query／2000ms handler、5秒恢复不变；丢失确认写恰1行且未重放、exit0、远端连接0、cleanup空。第一轮因本机不存在独立libpq公式路径而未启动native服务，两个私库清理正常；随后通过pg_config定位已安装PG17库验证成功，没有安装新依赖。以上确定性反例尚不能证明历史压测acquire_timeout由同一缺陷导致。

本轮owner WAIT索引补完后，现有source target22项与PG poll target11项通过，1个原有live opt-in未启用；独立只读复核未发现正常调用下的具体错误。严格Clippy（all-targets/all-features、-D warnings）、全workspace格式与debug CLI构建通过。Mac配置ASan/UBSan的TCP两项及PG11项通过，`detect_leaks=0`，不冒充Linux强制检测器。实际现有私库脚本再跑sync／async，两路均完成10000行、中文／NULL、原150ms超时、断连、确认丢失恰1行及原5秒恢复；exit0、远端连接0、cleanup为空，async unknown7／confirmed_cancel0／未确认回滚。该次CLI `40c2e42c1f8e86c4ce38896bc3a07f3af59adc09f7303994e6dc71650cbf37cc`，报告SHA `79d0c78d89b61659910fba6a7e929e46cdccae660ba2ab27262fee74594bb9f8`，是在86f基础加本轮dirty生产改动构建，不按HEAD冒称clean。

最新本机实库使用独立sync／async私库／代理、debug native C、4 event loops、4池连接／8 waiter；原acquire100ms／query150ms／handler2000ms、180秒外层及故障恢复5秒不变。两lane均passed，10000行2列、Unicode／NULL、超时、新读恢复、断连、写确认丢失均有实际结果；丢失确认的写恰1行，原SQL未重放。两次自然退出0、cleanup_errors均空、退出连接最终0。本机PostgreSQL17.11结果不能替代Linux16或三平台CI。

`pg_stat_activity`明确：async压力join后首次观察有5个远端session，其中4个仍在PgSleep、1个idle；原5秒窗口内观测为约0.207秒5个→0.739秒1个，非持续泄漏证明。压力观察与join共用原5秒，drain／进程退出／远端归零共用从drain请求开始的5秒，最多8次有界查询；不再要求close之后瞬间远端归零，也不延长窗口或确认回滚。该验收修改已提交86f并取得两次CI三平台12路实库通过，前一失败 `pg-async-connection-diagnostic.json` 保留。

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
| 86f push40024100907／PR40024106559 | push唯一失败为Windows Caddy rolling资源快照spare41线程>33，cleanup采样PID1956仍存在；PR全部必要项过。两次CI三平台PG sync／async共12路均实际passed | spare为supervisor6＋launcher6＋两个直接子12／17线程，总105<133；旧记录缺映像／创建时间，不能唯一判断两Ku实例重叠或PID归属。PR同检查通过、无遗留，不能覆盖push失败；不增加reserve |
| WAIT索引4ea→c1cc配对 | 首个control正式30秒60000/60000，fullCPU117.774117µs／正确、p99 7.48ms、5秒静止过；candidate首次100rps／30秒预热2999/3000，1个acquire_timeout即停止，未进入candidate正式窗 | 0个完整配对，不计算收益／bootstrap。RID0964695060001391计划13.910s、首发13.921261s、完成14.186446s，不能将客户总耗时当内部排队。各域0节流，现场lease／queue／线程未采，不能唯一定因；两代exit0／无强杀／域删除，candidate事后5秒账本静止未采 |

39c PR40015062381／push40015050574均failure；Rust workspace步骤、Clippy及已有detector通过，三平台包构建／包内小范围smoke有成功回执，但不能写成实库全矩阵通过。现有包smoke覆盖解包／安装CLI版本、hello、JSON、HTTP两loop的/ok／cpu／timer及正常drain；不含安装包PG运行。Actions外ZIP digest不同于内归档hash，完整39c包未在本机下载重算。最新d450 push40027977065／PR40027992019必要CI均成功；push三平台实际包及资产集合检查成功，分支发布步骤按规则跳过，不作为发布通过。该时点尚未独立验收实际安装包PG，新增workflow受OAuth权限限制；后续官方授权、f2b668d上传与实际门禁结果见第8节。

另一个有序退出修正限Java基准：同4.5秒绝对预算依次HTTP drain／在途0／CPU executor／PG pool／Vertx，严格匹配实际SIGTERM、PID／listeners、阶段／计数、无OOM／强制终止和cgroup清理才接受143；其他栈要求exit0。19项驱动回归及真实PG／timer在途退出通过，不把任意非零退出放宽。

86f Windows Caddy失败仅补现有夹具身份记录：Toolhelp映像名、选中及cleanup采样PID的实际映像路径和创建FILETIME；查询失败保留unknown，句柄finally关闭。不改变归属、资源门槛、清理策略，不新增HTTP轮询。既有Python测试23通过／1平台跳过；d450两次真实Windows CI相关检查成功，但未再次触发原异常，未将未知原因写成已修复。Mac配置TSan又通过TCP两项及真实HTTP多owner TCP一项；这不替代Linux必要gate。

d450 push的三平台PG原始artifact已下载核对API ZIP字节数及SHA：Windows 69597B、macOS 45684B、Linux 45258B，均匹配。每个平台原始报告的sync／async两lane均为passed、cleanup为空，共6路；CLI来自该CI的Cargo构建目录，不写成安装包PG通过。回执 `ci-d450b3e/push-pg-artifact-check.json`。

唯一后续599有界8秒诊断960/960正确、最大计划到完成29.74ms，未捕获所选异常；全局环覆盖1196条早期记录，不称trace整体完整。collector错用 `/proc/TID/stat` 的线程user/system拆分已剔除，仅独立schedstat可用于运行／就绪等待；未借此归因平台。诊断开销不入正式比值，服务／驱动／perf正常回收，临时C／ELF／脚本已清理。

历史最早3个504仍是“证据不足，无法唯一还原”；后续历史7次及旧正式30分钟未通过仍保留其版本／夹具范围。当前短测与不同路径的通过不改写旧根因，不追加盲目历史长压测。

## 8. 证据索引、缺口与交付边界

实际安装包PG入口已在本机执行：d450 push Mac ZIP／API digest相符，解包CLI `bc4de8917b290a8c089e68d9b4ab7641e4a26a1e327e390f349eaa6963026f64` 的版本／hello／JSON及真实sync／async均通过；postmaster0退出、进程树／session回收及临时根清理均确认。另将sync driver原180秒外层上限仅缩短至8秒，在实际HTTP readiness后受控停住，观察到期限终止并真实回收HTTP及PG，随后独立async lane通过。默认产品／SQL期限不变、没有重试。163项旧准备回归加同目录导入反例后，共164项（141过、23平台跳过）。实际还修复原runpy包装未设置script目录导致PG driver sibling import失败；未回收zombie在macOS的getsid/getpgid ESRCH已用真实子进程复现并由独立ps parent/group/Z核对处理。证据`evidence/installed-pg-gate-local-verification/verification.json` SHA `2b968da84686b31e3f8bc7514bcf4d2f925463b6a9d1065507c253a6d28e8350`。这验证当时的门禁实现，**不是最新产品或三平台最终交付**；本机阶段尚未上传新增workflow，后续实际CI状态如下。

后续官方设备授权已完成，实际API确认同账号的workflow scope，门禁提交f2b668d正常推送。e1ca的push40042849739及PR40042854151必要CI已通过；f2b668d push40046655519的Linux／macOS实际安装包sync／async四路也通过，包及CLI身份、进程回收均有原始报告。两个作业随后败在import-library目录清理：POSIX生成点号前缀，清理只接受横线前缀。Windows则在initdb前失败，内层目录的用户SID规则已正确设置，但新增外层临时目录仍只有保护ACL，PG降权后无法遍历。本次仅跟进目录前缀及新建Windows私有外层目录的已有权限处理；不更改产品或放宽系统父目录。原ZIP大小／API digest和失败日志保留于`ci-f2b668d/`；后续修订仍须实际三平台门禁，不能把四路通过写成整个作业或全矩阵通过。

d3b0c00 push40048521049在Windows bundle的新增独立live ACL单测中失败：原powershell.exe调用超过20秒，尚未分清启动／Get-Acl／Set-Acl阶段；三平台installed PG matrix因此整体跳过，不能当通过。该独立实测与必跑安装包实库门禁重复，本次仅删除AI新加的这一方法，4个契约回归及真实门禁每次外层ACL→initdb→SQL→回收路径保留，helper／20秒／expected outcomes均不改。168项离线回归145过、23平台跳过；这不是修好PowerShell超时的证据，后续必须以实际门禁结果为准。原日志和清理说明在`ci-d3b0c00/`。

3b push40049602257最终18个作业成功、1失败、发布跳过；Windows外层和两个内层ACL实际通过，owner和父SDDL不变。sync在编译超过原60秒后直接child已reap；async普通写入返回`execution_unknown`，不能推断回滚或重试。最终Job drained／进程树／临时目录清理通过，但没有中间编译后代快照，不能声称它们造成了未知写。发现独立源码缺陷：仅HTTP／PG清理成功就放行下一lane，未涵盖工具强制终止后的未确认后代；新增反例旧代码错误启动async，现以原异常上的类型标记和明确报告阻止下一lane，交由原外层owner先回收。编译增加已有`--verbose`，原60／5／20秒不改。对应三组171项：148过、23平台跳过。Linux／macOS两份小ZIP另经API字节及SHA核对，sync／async四路和cleanup均通过；原件及摘要见`ci-3b4e724/`。

已验证交付基线f833的push／PR必要作业均成功，push三平台实际安装包PG共6路passed，正常退出0、远端连接最终0且cleanup_errors为空。三份实库证据ZIP为Windows24800B、Linux36390B、macOS35927B，已核对API digest／大小、提交／目标／包／CLI身份；Windows外层及两lane内层ACL均通过，原owner与父SDDL不变，Job及临时根回收确认。证据`ci-f833ae5/summary.json`。这些结果不解释3b编译延迟或普通写unknown，也不证明d3的PowerShell超时根因已修复；发布仍按分支规则跳过，f833之后的修改不自动继承CI通过。

v4补齐五项Ku独立检查均200/200，原mixed失败继续保留。容量探索Ku512点2560/2560正确但p99越界；前100rps×3秒prime已203/300超100ms、计划发送p99约2.89秒，不能把480rps当可靠容量。Go32768点事件丢16099、87982未发出且driver节流，是无效测量。HTTP／JSON对象标needs_analysis，数组有序中断；全部测量服务域回收，仅私库保留。修复后warmup／prime复用原qualification，失败保存原报告并由原Service.stop回收，抛PreparationWindowFailure，目标窗口不启动；policy1进入身份、缓存和聚合，旧窗不续拼。三个反例旧失败、新harness24项通过；driver仍v5、算法／期限不变。

v5实际冻结已完成：产品候选仍为e1ca，431项业务输入与59项执行器文件分别记录身份，执行policy1；47条独立资格记录保留原出处和mixed失败，正式窗口导入0条。`verification.json`明确`freeze_completed=true`、`load_started=false`，冻结准备不冒称测量执行或性能通过。v4等待组原始168份文件留在guest，项目外只保存原汇总、逐档核对及路径／大小／SHA清单。用户确认没有独立Linux测试机，先完成本地代码与交付工作；当前不追加正式负载，不用无效窗口填表。

两项有界静态候选均未采用。仅拆算术错误冷路径使正常静态指令34→43、栈读3→3，未测速；普通Ku checksum函数封装使静态路径34→27、栈读3→0，但同布局两轮短配对CPU比0.992437／1.011216，无一致收益，不追加长测。候选首次输入布局误配在计算前退出，失败保留；后用相同相对布局重新生成，四窗各4096正确、自然退出／域回收。此为不同基准源码的诊断，不是编译器提速证明，未覆盖原正式表。证据见`current-validation/{coldmath-static-once,checksum-function-static-once}.json`及独立short-v1／v2原结果。

e1ca O09训练／留出及短观察的原始819成员归档SHA `fda4d35fc5ec6b25a30701513b579fbccbe4f32964628d86d01107ceb41674a1`。Ku normal／O3／PGO实际batch热路径约34／33／25条静态指令；PGO减少界限构造和栈加载，原溢出及期限规则仍在，但静态条数不是CPU成本占比。两个短配对的Ku纯PGO相对O3方向不一致，不能由此改变默认flags。Rust训练800次中6次读头超时，未形成合格use；后续13窗与外部浏览器交互可能区间相交，第一轮也有数秒采样空窗，均保留且不补测或当正式收益。独占重复构建缓存831044227B和一次性控制器已清理，22个执行文件SHA前后相同。详见项目外`o09-e1ca225/结论.md`。

正式矩阵新v3冻结实际Node入口同目录的5份源文件及成员，补上v2未登记的compute.cjs；40项离线检查通过，旧freeze不改写。旧c1cc L1保持原提交／boot下的观察：Ku batch源／C／ELF逐字相同，其他依赖的旧build proof181项与当前相同，但旧执行期未逐项记录实际Node worker路径及.NET共享runtime，未自动将整组改写为新矩阵。新golden共10项，9项通过；Ku normal在先ok200、JSON200后的mixed200中199正确、1个PG acquire_timeout，RID `2679895060000176`。本轮acquire为250ms，从操作创建起且包含后台建连；HTTP发送至返回277.423ms不等于内部等池耗时。各域未节流、无OOM、服务正常退出并回收；尚无PG阶段／owner线程现场，不能归为旧handoff缺陷。相关正式main矩阵尚未开始，保留失败，不增加超时或预热DB掩盖问题。 随后独立诊断C保留原ok→JSON→mixed顺序，5代mixed1000/1000（PG200/200）均正确；首代覆盖后台建连3次／waiter通知2次，创建到首次step最大0.358ms、到SQL发送最大18.706ms，无事件丢失。703次进程采样没有异常样本或用户态栈，不能解释原失败。5代自然exit0、域回收；探针／诊断C与ELF／控制器已删除，原C／ELF复核不变。

隔离VM已在无负载时正常停私库，稀疏盘32→128GiB；实际文件系统132011507712B、可用约118104334336B，CPU8／4GiB不变。私库PG16.15的1024行／schema hash与原一致、无强制停止；新boot／DB域另记，旧freeze／manifest不覆写，不将跨重启差异称因果收益。新矩阵冻结另补Java classpath、.NET publish、Node NODE_PATH及复用证明逐文件覆盖；53个离线反例过，仍需新freeze实际重哈希。

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
| c1cc Linux实际构建 | manifest `dbcf33a256182a7a720e4adf4fd3a72a13dca40433601ea5a1d08d4359100a62`；CLI `2e2f17ea9ccb6a9ef5bf910ab766c9f74e73bf6683645eb9aebb519434b3ffaa`；server ELF `b434f9ea178852fa5a756b1910162eabcd3f644edf5f12653d37edd0fdf64e8e`；raw20成员归档 `611806f8a41f03024ed3859cc322c8646e9ebc0d603b128363639a279fc461dd` |
| c1cc正式单核七轮 | `l1-c1cc-formal/summary.json` 40097628B，`d4d199706820503ed709c564110b50adc799cfee7b28d4cffe7bc4276cbd3444`；配对原件 `945124e0fdc7b08fe42402ebe166e5aa1d3196b70f6d1ac974b01aa13973559d`；传回核对 `4b263054b2506a7b640c68a4768ceeeb66a42423ddf28b23a93d85e1df463f4d` |
| PG等待交接修复 | `current-validation/pg-handoff-verification.json`，`9efd25010f510c36de22ab55189579402988177dd8f90d07b8796b14566a8555`；前后3反例、14回归、配置检测器、真实双lane与源码／CLI hash；不是历史错误定因 |
| WAIT索引A/B首次失败 | `net-active-index-ab/evidence.tar.gz`，1590581B，27项，`1e3da9710ba95c6afcd7b761f387b88e19665a4e22161a6aea91f22f47ccc203`；0完整配对，所有失败／全周期保留，临时控制器已删 |
| d450 push三平台PG原始artifact核对 | `ci-d450b3e/push-pg-artifact-check.json`，`f1c55fce46941da867a3cf3fe06c8e7e00cdb3af701f027eac99139ebe877fde`；3个ZIP外层digest／大小匹配，sync／async共6路passed且cleanup为空，范围为Cargo CLI |
| f833已验证交付基线 | `ci-f833ae5/summary.json`，`124ff372c494f8097bbb4c42373e48dca576baa37f2b01dc7ecf13beef68ec6e`；push19／PR12必要作业成功，三平台实际包PG共6路通过，不覆盖后续修改 |
| v4四核等待组核对 | `current-validation/waiting-c4-verification.json`，`d5ced21add66edf94cfed8f50f5c597ef6610856e63442aa4cbddcb7617443ad`；40档13过／8未过／19阻塞，8192未验收，原始文件清单独立保留 |
| v5实际冻结核对 | `evidence/matrix-runner-v5-preparation-policy/verified-freeze/verification.json`，`ae1521a81415056b67ba0140313bb7123239d2f908601b4054cbe6f21c2eb98f`；独立业务／执行器身份，未启动负载、未导入正式窗 |
| 8ad Windows失败外ZIP | `72e3c4caeae6d32aa4d73f2623f7ca2e89ad546ee10675a22aa7558975da7429`，API digest与下载匹配；不是release内包hash |

尚需完成：最新候选实际Linux C／ASM成本闭环及有效单变量收益；不依赖失效窗口的正式A／B容量和同速率核心点；normal／reset、8192等待、600秒混合、真过载／恢复。f833已完成三平台必要CI与同源实际安装包PG验证；之后的本地产品修改必须单独验证，不能套用该基线结果。用户选择先完成其余本地工作，环境条件未解决前不追加正式压测；结果未跨门槛保持未验收，不能以测试数、包构建或其中一栈较快代替目标。

JSON保持旧默认；暂停的通用值复制／释放后续分析与广泛AST分析不展开；无关36次进程内存验证不重跑。本轮没有部署官网、修改旧r1标签／资产、合并或公开Release，未操作真实业务库。保留用户dirty、原源码／测试／历史证据；自己的临时探针、脚本、构建副本及进程用完清理，必要永久回归／基准保留。
