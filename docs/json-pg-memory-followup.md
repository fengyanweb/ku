# 0.0.19 后续：JSON、PG 与固定内存

[English](json-pg-memory-followup.en.md) · 2026-10-29

**状态：性能目标未完成。** 保留一项已复现的启动配置修复；PG 协议实验未显示收益，已经撤回。没有修改 JSON 默认引擎、暂停的 Value/AST 模块、发布标签或网站部署。

## 代码与交付边界

- 报告冻结版本 `d8c33bca3c629ce82d0d35709c6931f16c86d05b` 与发布版本 `73e9f2fef6861b0bc1ce441471726b926a8a174b` 的本次运行时路径相同。A 使用后者正常 CLI Release，helpers/text/async/int、C11 O2；没有恢复 A3/A4、O3/direct 或 yyjson。
- 开发分支 `codex/v0.0.19-json-pg-memory`，运行时修改不合入 main。主工作目录的旧版 HEAD 和未提交改动保留。
- 网站单独通过 [PR #9](https://github.com/fengyanweb/ku/pull/9) 合并到 main：`6aedaf4b0c4e68612c49a23a8607f0c9901bb4ff`。只包含网站源码，930 页构建及链接、资产、手机、示例核对结果与上传源码 SHA 对应；没有部署。

## JSON 成本：已确认与未知

`/json` 是同步 HTTP handler，不经过计算路由的 Task 状态机。路径为 HTTP body 所有权移交 → decode → counter 标量 → take → 根清理 → envelope → encode → http.text → 释放。`take("data")`、UTF8 连续段、整数字段输出和 body 移交原本已经生效，不能列为新增收益。

复用既有分配 hook 的机制夹具：1,048 字节对象，32 字节 owned RID。它复用实际生成 Transform；RID 与 HTTP 驱动的长度分别记录，以下不是 HTTP 全进程分配账。

| 阶段 | 分配/扩容调用 | 请求字节之和 | 显式 memcpy 字节 |
| --- | ---: | ---: | ---: |
| decode | 28 | 7,800 | 892 |
| counter、take、根清理 | 0 | 0 | 0 |
| envelope，含 RID 拥有值读取 | 3 | 696 | 32 |
| encode | 4（首次加三次增长） | 3,264 | 1,087 |
| 总计 | 35 | 11,760 | 2,011 |

峰值受追踪 payload 8,152 字节；对象、数组及转义变体最终 live allocation 均为零。申请字节包含 realloc 的新请求大小；复制账不含 libc 内部搬迁、结构赋值、HTTP 投影或传输。它不是 RSS，也不是整个服务不存在泄漏的证明。

正常与带符号诊断 ELF 的 `.text` SHA 相同：`b5aacdd91c550ae738b4f137fd82beefb424b18b69d5f6afd23f58dd2a9a5d68`；生成 C SHA：`d2456e0c438a221d950f3f01744193f0af74cd4c6bf144404a996695719a5073`。最终普通 A ELF 为 `d1e0b719e542172a6806c0398f87e28bc533b4ca743dd342581b2151910eed56`，profile ELF 为 `c2f7d6b55cc8390d7ad29d0942fae8506a2315e201d74b98c6a8e9ab63283170`。早期 objcopy 导出曾改写普通 ELF 元数据，已由正常 CLI 恢复；未将旧 SHA 冒充实际运行产物。

低开销 JSON profile 共 95 个 SELF 样本、零丢样：内核 45、Ku 26、libc 24。JSON 内部仅 9 条捕获栈，不能排序细小优化；部分 libc/调用者未解析，保留未知。第一次采集丢失 11.42%；第二次 after 窗口驱动失效并出现超时。原始失败窗口保留，没有挑选 before/sample 当配对成绩。没有据此修改 JSON。

## PG 实验：无可靠收益，撤回

`f2ff379` 仅将固定 `DISCARD ALL` 改走 simple query，业务 SQL 仍绑定参数；同一 lease、原期限、收尽到 NULL、IDLE/UTF8、失败驱逐及 owned 结果寿命不变。默认 Rust 使用 deadpool-postgres Fast，不做该 reset；相同严格 reset 的 Rust 模式必须单列，不能当成默认结果。

正常 CLI 构建的一次筛查，200/s、预热 5 秒、测量 10 秒、4 服务核、驱动与私库分别限域，A 停止并清空后才启动 B：

| 指标 | 原 Ku A | 协议候选 B |
| --- | ---: | ---: |
| 正确 / 计划 | 2,000 / 2,000 | 2,000 / 2,000 |
| 服务 CPU µs/正确请求 | 317.843 | 319.273 |
| PG 域 CPU µs/正确请求（独立采样窗口） | 294.115 | 286.603 |
| 服务 memory.current 稳态中位字节 | 4,085,760 | 4,044,800 |
| scheduled→complete p99 桶上界 ms | 3.14 | 2.86 |
| 发送延迟最大 ms | 7.957292 | 8.743190 |
| 驱动后补充 CPU 秒 | 0.000117 | 0.000149 |

两边驱动有效、零错误、静默计数归零、自然退出及 cgroup 回收通过。B/A=1.00450，未显示服务 CPU 改善；单对结果不提供显著性、稳定内存收益或 Rust 排名。默认代码在 `bee69ea` 撤回，实验保存在 `codex/v0.0.19-pg-reset-experiment` 和提交历史。没有重跑等待好成绩。

复用回归：21 项通过，1 项实库 opt-in 未在该命令启用；之后复用原 LIVE_LOOPBACK 夹具，在候选正常生成 C 上单独实库执行通过，覆盖参数、owned Unicode/NULL、多结果、错误、期限丢连接及后继查询。协议调用计数由已有 stub 验证，不冒充实库 wire trace。业务 SQL 不重试。

PG 另有 1,142 个 on-CPU 样本、零丢样；采样窗口 CPU 明显高于前后控制，不能计为成绩。query/reset 各自 CPU、pool 等待及额外推进的贡献仍不能由这些总量唯一拆出，不将 PG 服务器 CPU 或 poll 数当成服务热点占比。

## 固定资源与实际修复

23 个线程来自主线程 1、输出 1、事件循环 4、同步 HTTP worker 16、Task 后台 worker 1；external Task driver 没有再建一套线程。同步 JSON 和普通路由需要 handler pool，不能为测试禁用它。睡眠线程不等于忙轮询。

空闲样本 memory.current=3,448,832 字节，anon=2,211,840，kernel=974,848，其中 kernel_stack=360,448、pagetables=204,800、slab=369,576。子项不能与 kernel 重复相加；各内核计数可能分批更新。进程 RSS=8,368 KiB、PSS=4,002 KiB，与 cgroup 收费口径不同。第一轮客体 pswpin/out 增量均零，服务 smaps Swap=0；同期宿主确有大量换页，不能归成 Ku 被换出。固定结构容量与驻留字节仍不能逐项一一归属，没有据此盲目裁池或宣称已降低内存。

**修复 `b8f3497`：** 显式两个事件循环、未配置阻塞线程数时，旧默认先取两个再扣除 Task 后台名额，导致拒绝启动。HTTP 默认现在把该名额纳入原总线程预算；显式配置不足仍拒绝，普通 Task 根入口不变，基准 B17 不变。六个配置组合及现有启动失败、pure-native 200/404/405、混合 async/文件/期限/退出回归共 7 个测试通过，格式与 diff 检查通过。未增加生产库字段或 migration。

## 未完成项与下一条证据

| 项目 | 本轮结论 | 仍缺什么 |
| --- | --- | --- |
| JSON 对 Rust CPU/内存追平 | 未证明；无新默认优化 | 稳定窗口的更充分 on-CPU 样本，定位真实大头后再选候选 |
| PG 默认与严格同合同追平 | 未证明；协议候选撤回 | query/reset、libpq/Task/reactor 的动态成本；新的有依据候选 |
| 固定内存下降 | 未证明；完成来源审计与配置 bug 修复 | 各结构实际驻留/分配器保留归属，保留同等能力的方案 |
| 最终五组 A/B/R 及公共路径守护 | 未开展 | 不再发生已观测驱动失准的环境；不能用单对筛查替代 |
| 必要 CI | 以开发分支实际提交检查为准 | 网站 main 的 macOS 测试前提失败需单列，不能称全套通过 |

网站 main 的 [CI](https://github.com/fengyanweb/ku/actions/runs/40828377118) 中 Windows/Linux 全套、lint、sanitizer 和安装包/PG 项成功；macOS 唯一 lib 测试在“慢 body 未进入”的断言失败，随后证据上传因文件未生成失败。产品错误返回及 400ms 上界断言已经通过；未记录是否 accept/header/首字节失败，所以具体前提失败原因仍未知，不能补称产品期限错误。未为此扩大本次产品修改。

## 证据与清理

本机独有证据保存在 `/Users/mac/Documents/Ku/实施/JSON_PG_后续_2026-10-28/`：`plan.json`、`json-allocation-ledger.json`、JSON/PG profile 分析、宿主时间序列，以及 `raw/` 内的原始请求、perf、生成 C、ELF、窗口和 SHA 清单。压缩只改变存储形式，保留解压后的原 SHA；原始 stack dump 私有保存，不原样上传 Git。没有把私库连接信息放入本报告。

本轮脚本、探针和重复构建产物在验证后清理；永久回归使用原测试文件。保留失败证据和现有用户源码/测试/未提交改动。此报告没有将历史七语言成绩、局部计数或本次短窗包装为完整性能或生产验收。
