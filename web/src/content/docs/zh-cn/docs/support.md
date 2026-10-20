---
title: 平台与后端支持
description: 区分 Ku 已发布平台、解释器、默认 runner、native C、Task 子集与 LLVM 的能力。
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'docs/syntax.md',
    'docs/v0.0.18.md',
    'docs/v0.0.18-worklog.md',
    'docs/protocol-foundation.md',
    '.github/workflows/native-three-os.yml',
  ]
---

本文档对应 v0.0.18 实验预发行版的冻结范围；实际公开状态与下载资产以 GitHub Release 为准。

## 已发布的 CLI 平台

v0.0.18 实验预发行包覆盖 Windows x64（MSVC）、Linux x64（GNU）、macOS arm64（Apple Silicon）。每个平台使用独立归档，没有一个二进制能通用于三个系统。其他平台未核实安装资产，见[安装与下载](/zh-cn/docs/install/)。

## 执行方式

| 能力                                 | 解释器 `ku run`        | 默认 `ku build` runner | 同步 native C                     | 开发中 native Task     | LLVM 文本原型  |
| ------------------------------------ | ---------------------- | ---------------------- | --------------------------------- | ---------------------- | -------------- |
| 基础语言、函数与控制流               | 有实现                 | 使用解释器路径         | 已实现子集                        | 有限 typed 子集        | 有限原型       |
| JSON                                 | 支持                   | 支持                   | 支持                              | 不支持                 | 不支持         |
| 完整本地 import graph 的源码无关运行 | 从源码加载             | 不支持；仅嵌入入口     | 在生成期展开                      | 在已支持子集中展开     | 不作部署承诺   |
| async / await                        | 有实现与资源限制       | 使用解释器路径         | 转入专用子集或拒绝                | 部分支持，非完整 async | 不支持完整能力 |
| PostgreSQL / Redis / MySQL client    | 不支持连库             | 不提供解释器连库       | native-only                       | 未列入通用等待能力     | 不支持         |
| TCP / TLS client                     | 不提供对应 native 路径 | 不提供对应 native 路径 | generated C 实现，TLS 需匹配 pack | 非通用网络等待完成声明 | 不支持         |

默认 build 要求匹配的 Rust 工具链和 linker；native C 要求目标 C 工具链。`ku check` 通过不能证明两种构建都成功。LLVM 命令存在也不等于完整可发布后端。

## native Task 当前做到哪里

当前开发分支有受限的 `async fn main(): null!` 源码路径，覆盖已列出的 primitive、字符串、平面数组、单层 Result、Task start/move/await、部分控制流，以及有界文件/目录后台操作。它不是已发布 v0.0.18 的完整 async 后端。

网络通用 Task 等待、完整 M:N、任意动态堆组合、所有 async 捕获/函数值组合和完整长期 soak 仍未完成。具体接受与拒绝范围见[并发合同](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/concurrency.md)。

## 数据库与网络边界

数据库统一使用 `module.client(config)?` 和 receiver 方法，内部维护有界池。实库验证按驱动、系统和当前提交分别验收，fake ABI 或链接成功不能替代真实查询验证。

Redis 仍为明文 TCP；MySQL 未提供跨 Oracle/MariaDB 一致的 fail-closed 证书验证配置。它们只应直连 loopback、受控私网或经验证的 TLS tunnel。PostgreSQL 远程连接需要显式 `sslmode=verify-full` 与可信根证书。`std.net` 的 native TLS 实现不代表 HTTP、Redis 或解释器已经复用它。

## 自举与版本兼容

Ku 自举 Lexer 和 Parser 已有有界子集及差分验证；完整 Parser、checker、IR 与 backend 的自举尚未完成，Rust 仍承担 bootstrap 工具链。0.0.x 不承诺源码或生成 C/FFI ABI 向后兼容。

工作日志和历史 CI 只证明对应范围与提交。查看[发布记录](/zh-cn/releases/)区分已发行变化和开发内容；本网站未把当前工作分支的全部 CI、生产负载或数据库验收写成已通过。

## v0.0.18 冻结范围与已知限制

本版只交付已经实现并验证的功能与修复，仍为**实验预发行版**，不代表生产就绪。[v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18) 的实际记录决定公开状态、下载资产和校验信息；发布任务跳过不能当作发布通过。

- JSON 保留旧底层实现作为默认；`json.encode` / `json.decode` 复用 `stringify` / `parse` 入口，包含 `1e20` 浮点往返修复。yyjson 替换与性能探索已经结束。
- Task/native async 保持合同列出的有界子集。关闭准入与回收修复不等于完整 M:N 或全部网络可挂起。
- HTTP 包含有界 socket 就绪等待、队列、响应写回、keep-alive 及可选有界诊断记录；不因此宣称所有负载验收通过。
- `ku supervise` 管理完整程序 worker 的准入、排队字节、输出及清理；不是每请求或每 Task 一个进程，普通 `ku run` 不会自动启用。

**HTTP 已知问题：历史出现过 504（包括一次 7 请求异常），本轮 60 秒与 180 秒短测未复现，根因未确认，正式 30 分钟稳定性验收尚未通过。** 用户已接受将其列为本版已知问题，不再单独阻塞发布；不得改写成已修复或长期稳定性验收通过。最早三次历史 504 证据不足，无法唯一还原。

**移入后续版本、未标完成：**完整 M:N、通用 Task 网络调度、数据库完整平台矩阵及进一步长期负载验收。通用值复制/释放后续分析和 AST 分析继续暂停；不重复无关的内存限额验证。

### 进程内存限制的真实边界

| 平台    | `ku supervise` 条件与指标                                                                                                                                                                                               |
| ------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Linux   | 需要可写且已委托的 cgroup v2 domain 根目录，启用 `memory`、`pids`，提供 `memory.max`、`memory.swap.max`、`memory.oom.group`、`pids.max`、`cgroup.kill` 等接口。按内核 cgroup 计费内存限制；缺少条件时在应用执行前拒绝。 |
| Windows | 嵌套 Job Object 限制单 worker 与 worker 合计的提交虚拟内存，指标不是 RSS；应用初始线程保持暂停，核对初始提交量与 Job 配置后才放行。宿主策略可能拒绝 Job 分配。                                                          |
| macOS   | 原生不支持此隔离模式，在应用启动前明确拒绝；需要时可使用满足条件的 Linux 主机或 VM，不提供自动 VM 桥接。                                                                                                                |

worker 资源域覆盖适用的 Ku、MySQL、libpq、TLS 等第三方库分配，但 Linux 计费内存与 Windows 提交量不是同一指标。Linux `memory.max` 允许特定情况下瞬时越限，不能承诺任意时刻所有物理内存绝不超过一个固定字节数。

**协调器自身没有 OS 内存硬限制，控制预留和有界准入不是 RSS 硬限；单 Task 完整堆未精确计量。** 共享连接池、缓存与外部库内存不能精确分摊到每个 Task。外部数据库、宿主/VM 与构建编译器不在 worker 限额内，不宣称整个系统内存绝对封顶。

OS 终止超限 worker 后，协调器报告失败，确认回收后才归还预算；部分分配可提前拒绝，但第三方库耗尽并不保证转成普通 Result。**数据库不自动重试**，worker 退出不证明数据库回滚，副作用不确定时如实报告。

详见[受控进程合同](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/process-execution.md)、[并发合同](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/concurrency.md)和 [HTTP 验收记录](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/http-stability-acceptance.md)。各平台 CI、安装验证只证明实际报告的目标与范围，不等于数据库完整平台矩阵完成。

## 开发时运行受控程序

先准备依赖和普通 `hello.ku`。Linux 的 `KU_CGROUP_ROOT` 必须由管理员或服务管理器提供真实可委托根，不能用普通目录代替：

```sh
ku supervise --worker-memory-bytes 134217728 --total-memory-bytes 536870912 \
  --max-workers 2 --max-queued-jobs 32 --max-queued-bytes 262144 \
  --max-output-bytes 1048576 --deadline-ms 30000 \
  --cgroup-root "$KU_CGROUP_ROOT" -- ./hello.ku
```

Windows 沿用数值参数，省略 `--cgroup-root`。字节数和毫秒数用十进制数字；`--` 后每个入口是完整程序，不是请求路由或通用网络调度接口。命令依次输出 configuration、execution、summary JSONL，任一程序失败则最终非零退出。native 程序先构建，再通过 `--native` 提交；编译器不在 worker 限制内。
