---
title: 发布记录
description: 'Ku v0.0.18 experimental prerelease scope, downloads and known limitations.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  - docs/v0.0.18.md
  - docs/v0.0.18-worklog.md
  - docs/history.md
  - Cargo.toml
slug: zh-cn/v0.0.18/releases
---

本文档对应 v0.0.18 实验预发行版的冻结范围；实际公开状态与下载资产以 GitHub Release 为准。

## v0.0.18 冻结范围与已知限制

本版只交付已经实现并验证的功能与修复，仍为**实验预发行版**，不代表生产就绪。[v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18) 的实际记录决定公开状态、下载资产和校验信息；发布任务跳过不能当作发布通过。

- JSON 保留旧底层实现作为默认；`json.encode` / `json.decode` 复用 `stringify` / `parse` 入口，包含 `1e20` 浮点往返修复。yyjson 替换与性能探索已经结束。
- Task/native async 保持合同列出的有界子集。关闭准入与回收修复不等于完整 M:N 或全部网络可挂起。
- HTTP 包含有界 socket 就绪等待、队列、响应写回、keep-alive 及可选有界诊断记录；不因此宣称所有负载验收通过。
- `ku supervise` 管理完整程序 worker 的准入、排队字节、输出及清理；不是每请求或每 Task 一个进程，普通 `ku run` 不会自动启用。

**HTTP 已知问题：历史出现过 504（包括一次 7 请求异常），本轮 60 秒与 180 秒短测未复现，根因未确认，正式 30 分钟稳定性验收尚未通过。** 用户已接受将其列为本版已知问题，不再单独阻塞发布；不得改写成已修复或长期稳定性验收通过。最早三次历史 504 证据不足，无法唯一还原。

**移入后续版本、未标完成：**完整 M:N、通用 Task 网络调度、数据库完整平台矩阵及进一步长期负载验收。通用值复制/释放后续分析和 AST 分析继续暂停；不重复无关的内存限额验证。

### 进程内存限制的真实边界

| 平台 | `ku supervise` 条件与指标 |
| --- | --- |
| Linux | 需要可写且已委托的 cgroup v2 domain 根目录，启用 `memory`、`pids`，提供 `memory.max`、`memory.swap.max`、`memory.oom.group`、`pids.max`、`cgroup.kill` 等接口。按内核 cgroup 计费内存限制；缺少条件时在应用执行前拒绝。 |
| Windows | 嵌套 Job Object 限制单 worker 与 worker 合计的提交虚拟内存，指标不是 RSS；应用初始线程保持暂停，核对初始提交量与 Job 配置后才放行。宿主策略可能拒绝 Job 分配。 |
| macOS | 原生不支持此隔离模式，在应用启动前明确拒绝；需要时可使用满足条件的 Linux 主机或 VM，不提供自动 VM 桥接。 |

worker 资源域覆盖适用的 Ku、MySQL、libpq、TLS 等第三方库分配，但 Linux 计费内存与 Windows 提交量不是同一指标。Linux `memory.max` 允许特定情况下瞬时越限，不能承诺任意时刻所有物理内存绝不超过一个固定字节数。

**协调器自身没有 OS 内存硬限制，控制预留和有界准入不是 RSS 硬限；单 Task 完整堆未精确计量。** 共享连接池、缓存与外部库内存不能精确分摊到每个 Task。外部数据库、宿主/VM 与构建编译器不在 worker 限额内，不宣称整个系统内存绝对封顶。

OS 终止超限 worker 后，协调器报告失败，确认回收后才归还预算；部分分配可提前拒绝，但第三方库耗尽并不保证转成普通 Result。**数据库不自动重试**，worker 退出不证明数据库回滚，副作用不确定时如实报告。

详见[受控进程合同](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/process-execution.md)、[并发合同](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/concurrency.md)和 [HTTP 验收记录](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/http-stability-acceptance.md)。各平台 CI、安装验证只证明实际报告的目标与范围，不等于数据库完整平台矩阵完成。


## v0.0.17 · 实验预发行版

GitHub 发布于 **2026-10-18（America/Los_Angeles）**，UTC 时间为 `2026-10-19T03:51:10Z`。该版本 `prerelease=true`、`draft=false`；当前没有已核实的稳定发行版。

主要变化：

* 同步只读借用参数 `&name: T`，调用仍传普通实参；普通 owned 参数保持 move 规则。
* Ku 自举 Parser 子集增加带花括号的 `while`，并保留有界深度与语句数量约束。
* 修正跨平台测试工具的 stdout 断开验证；这不增加新的业务 API。
* 发行归档携带默认 runner 构建所需的匹配 Rust 依赖与目标 TLS pack，仍需对应工具链。

下载覆盖 Windows x64、Linux x64、macOS arm64，7 个 Release 资产包括三个平台归档、三个 manifest 与 `SHA256SUMS`。参见[安装与下载](https://github.com/fengyanweb/ku/releases/tag/v0.0.17)和[完整发行说明](https://github.com/fengyanweb/ku/releases/tag/v0.0.17)。

### 兼容性与限制

Ku 仍为实验项目，不可用于生产；0.0.x 不承诺源码及生成 C/FFI ABI 向后兼容。相关 native 产物应随版本重编译。async borrowed 参数、完整自举和完整 native async 不在本次发布能力内。

默认 runner 只嵌入入口，导入模块仍依赖原源码位置。数据库为 native-only，实库、TLS 和 session 状态边界应按对应驱动说明核对。项目自身仍未授予开源许可证。
