---
title: 平台与后端支持
description: 区分 Ku 已发布平台、解释器、默认 runner、native C、Task 子集与 LLVM 的能力。
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-28'
sources:
  [
    'docs/syntax.md',
    'docs/v0.0.19.md',
    'docs/v0.0.18-worklog.md',
    'docs/protocol-foundation.md',
    '.github/workflows/native-three-os.yml',
    'src/service_supervision.rs',
    'src/ir/task_lower.rs',
    'src/backend/c_http_runtime.c',
    'src/backend/c_task_pg_runtime.c',
  ]
---

## 选择平台与执行路径

按[安装说明](/zh-cn/docs/install/)和实际[发行资产](https://github.com/fengyanweb/ku/releases)选择系统与架构对应的 CLI。源码构建、CI 作业或隔离安装预演不等于已发布相应平台的下载包。Linux ARM64 RC 预演只覆盖样例服务及其依赖；真实应用、生产 SLO 与灰度验收需要单独完成。

| 能力                       | 解释器 `ku run` / 默认 runner             | Native C                         | Native Task 路径                                                 |
| -------------------------- | ----------------------------------------- | -------------------------------- | ---------------------------------------------------------------- |
| 语言与本地 import          | runner 嵌入入口，但仍需本地 import 原源码 | 生成时展开受支持的源码图         | 受限的带类型子集                                                 |
| JSON                       | 通用编码与解码                            | 通用同步编码与解码               | 只编码标量对象字面量，不提供通用解码                             |
| HTTP                       | 客户端与同步 handler                      | 同步服务端子集，包含 `http.json` | 具名 async handler 及受支持的请求/响应类型                       |
| PostgreSQL / Redis / MySQL | 没有数据库客户端实现                      | 有界连接池，需匹配目标平台的依赖 | 受支持 async HTTP handler 内的 PG query 挂起，不是通用数据库异步 |
| 时间                       | 日历、Duration 与等待                     | 部分时钟接口                     | 整数毫秒 sleep                                                   |
| TCP、TLS 与文件 I/O        | 见各模块说明                              | 部分 native 接口                 | 仅明确支持的降低路径，不是任意网络挂起                           |

参数、返回值、错误和完整示例见 [HTTP](/zh-cn/docs/stdlib/http/)、[JSON](/zh-cn/docs/stdlib/json/)、[时间](/zh-cn/docs/stdlib/time/)与[数据库](/zh-cn/docs/stdlib/database/)。类型检查与后端编译是两个步骤。默认 runner 构建需要匹配的 Rust 工具链及发行归档布局；native 构建需要目标 C 编译器和相应原生库。LLVM 文本后端仍为有限原型。

## Task 边界

调用 async 函数会提交任务并返回一次性 Task 句柄；`await` 取回结果，不规定任务何时首次执行。native Task 路径支持已列明的基础类型、字符串、平面数组、Result、Task 启动/移动/等待、定时器和部分有界 I/O，也支持具名 async HTTP handler 及文档限定的 PG 捕获路径。这不代表完整 M:N、任意 async 闭包/捕获组合、通用 JSON 操作或通用网络调度器。

输入所有权、取消、共享清理截止期限和操作子集仍然有效。算术 Task 成功不能证明整个库都可用于 Task。请结合 [Task 指南](/zh-cn/docs/guide/concurrency/)和[并发合同](https://github.com/fengyanweb/ku/blob/main/docs/concurrency.md)阅读。

## 应用预算与进程限制

以下控制覆盖不同的资源：

| 控制                  | 范围与边界                                                                                  |
| --------------------- | ------------------------------------------------------------------------------------------- |
| HTTP service 选项     | 该服务的连接、活动/等待请求及受控传输缓冲，不是整个用户堆                                   |
| Task 与共享数据库预算 | 运行时管理的槽位、等待操作和受跟踪分配；共享池及外部库不能精确归属到每个 Task 的全部堆      |
| 普通模式 `ku service` | 管理一个常驻 native HTTP 实例，提供认证的健康/排空/指标与有界重启策略，不附带 OS 内存硬限制 |
| `ku supervise`        | 管理有界的完整程序作业，不是 HTTP 请求调度器                                                |
| 显式 OS 内存模式      | Linux cgroup v2 计费内存或 Windows Job 提交内存，需满足下表条件                             |

普通 `ku service` 支持 Linux、macOS 和 Windows。配置前请阅读 `ku service --help` 与 [CLI 指南](/zh-cn/docs/tools/cli/)。它不会自动约束无关的 `ku run`。当前没有公开 `app.stop()` 方法；受监督 native 服务的健康/排空控制与解释器 listener 的 `close()` 生命周期不同。fail-stop 退出（包括相关清理失败时的 native 退出码 80）属于服务失败，需要有限的监督策略。

| 主机    | 显式 OS 内存模式                                                                                                                                                                |
| ------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Linux   | 需要委派的 cgroup v2 domain，可写 memory/pids 控制，包含 `memory.max`、`memory.swap.max`、`memory.oom.group`、`pids.max`、`cgroup.kill`。缺少权限或控制接口时在应用执行前拒绝。 |
| Windows | Job Object 限制提交虚拟内存而非 RSS；应用执行前检查初始提交量及 Job 归属，宿主策略可能拒绝嵌套。                                                                                |
| macOS   | 拒绝 native 内存硬限制模式；普通服务监督是独立支持的模式，不自动桥接 Linux VM。                                                                                                 |

Linux 计费内存、Windows commit 与 RSS 不能混用。Linux 限制可能短暂超出，不保证宿主全部物理内存在每个瞬间都有绝对上限。协调器、外部数据库、代理、宿主/VM 和构建编译器不在被管理 worker 的内存域内。固定控制元数据也实际占用进程内存。

准备真实的 Linux 委派目录与基础 `hello.ku` 后，可用以下完整程序限制：

```sh
ku supervise --worker-memory-bytes 134217728 --total-memory-bytes 536870912 \
  --max-workers 2 --max-queued-jobs 32 --max-queued-bytes 262144 \
  --max-output-bytes 1048576 --deadline-ms 30000 \
  --cgroup-root "$KU_CGROUP_ROOT" -- ./hello.ku
```

Windows 省略 `--cgroup-root`。数值使用十进制字节或毫秒。`--` 后是完整程序输入；native 程序先构建，再以 `--native` 提交，编译器不计入 worker 限制。失败 worker 只有确认清理后才退还预留；分配失败或外部库被终止不保证成为普通 Result。

## 数据库、传输与失败边界

数据库客户端仅支持 native。真实服务器验证只适用于相应驱动、平台、依赖和源码身份，链接成功不等于查询成功。远程 PostgreSQL 需要显式 `sslmode=verify-full` 及可信证书。Redis 为明文 TCP；MySQL 在 Oracle/MariaDB 间没有统一的失败即关闭证书/主机名校验合同。需要加密时使用受控本地/私网或独立验证的隧道。`std.net` TLS 不代表所有库都复用 TLS。

**数据库语句不自动重试。** 超时、取消、worker 退出和连接重置都不证明回滚。先保留不确定结果，通过业务标识对账，再决定是否人工重试。

## 兼容性与当前限制

Ku 仍在开发中，不承诺完整自举检查器/IR/后端、通用 Task 网络调度或完整数据库平台矩阵。Rust 仍提供 bootstrap 工具链。0.0.x 不保证源码或生成 C/FFI ABI 兼容。

JSON 保留原默认实现，yyjson 替换未作为默认。局部运行时修正、健康短测或隔离 RC 预演，不代表延迟、过载、容量及长期稳定性验收全部通过。慢请求与历史超时证据仅适用于对应负载，根因未确认时不能推断由宿主或应用造成。

[发布记录](/zh-cn/releases/)列出已发布变化，请区分实测范围与真实业务生产就绪状态。本站不宣称吞吐排名、对 Rust 的全面优势或全项目无泄漏。
