---
title: 平台与后端支持
description: 区分 Ku 已发布平台、解释器、默认 runner、native C、Task 子集与 LLVM 的能力。
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/syntax.md',
    'docs/v0.0.18.md',
    'docs/v0.0.18-worklog.md',
    'docs/protocol-foundation.md',
    '.github/workflows/native-three-os.yml',
  ]
slug: zh-cn/v0.0.18/docs/support
---

Ku 0.0.18 仍处于实验阶段，暂不建议用于生产环境。

## 发布平台

当前提供 Windows x64（MSVC）、Linux x64（GNU）、macOS arm64（Apple Silicon）三个平台的预构建包。各平台归档独立，见[安装与下载](/zh-cn/v0.0.18/docs/install/)。

<span id="execution"></span>

## 执行后端

| 能力             | 解释器 `ku run` / 默认 runner         | 同步 native C                                       | 受限 native Task           | LLVM 文本原型  |
| ---------------- | ------------------------------------- | --------------------------------------------------- | -------------------------- | -------------- |
| 函数与控制流     | 已实现语言路径                        | 明确列出的子集                                      | 有限 typed 子集            | 有限原型       |
| JSON             | 支持                                  | 支持                                                | 不支持                     | 不支持         |
| 本地 import      | 运行时读取依赖源码；runner 只嵌入入口 | 生成期展开完整本地导入图                            | 在已支持子集中展开         | 不作为部署后端 |
| async / await    | 一次性 Task 与资源限制                | 使用专用 Task lowering 或拒绝                       | 部分支持，非完整 async     | 不支持完整能力 |
| HTTP             | 客户端与服务端；`bind/run/close`      | 服务端 `listen` 子集，无 `http.json` 或模块级客户端 | 没有通用 Task HTTP 接口    | 不支持         |
| 数据库           | 不支持连库                            | native-only client                                  | 没有通用异步数据库调用     | 不支持         |
| TCP / TLS client | 不提供对应 native client              | 支持；TLS 需要匹配 pack                             | 局部句柄通过固定后台池等待 | 不支持         |

`run/check` 不要求 Rust/C 编译器。默认 build 需要匹配的 Rust 1.89.0、linker 和整套随包依赖；native C 需要目标 C 工具链。前端检查成功不表示所有后端都能编译。详见 [CLI](/zh-cn/v0.0.18/docs/tools/cli/)、[HTTP](/zh-cn/v0.0.18/docs/stdlib/http/)和 [Task](/zh-cn/v0.0.18/docs/guide/concurrency/)。

## 重要限制

- native Task 支持整数 timer、已列出的文件/目录和局部 TCP/bytes 等操作。后台等待不等于全部网络 readiness、任意库或全部语言组合都可挂起。
- HTTP 使用单 I/O owner 和独立的 handler 执行路径。
- 数据库通过 `module.client(config)?` 使用有界池。Redis 为明文 TCP；MySQL 没有跨 Oracle/MariaDB 一致的证书与主机名验证配置。使用 loopback、受控私网或经验证的 TLS tunnel；远程 PG 使用 `sslmode=verify-full` 与可信根证书。
- 自举 Lexer/Parser 仍是有界子集，Rust 继续承担 bootstrap 工具链。0.0.x 不承诺源码或生成 C/FFI ABI 兼容，更新后应重编译 native 产物。

## 已知问题

### HTTP 504

历史出现过间歇性 HTTP 504。短测未复现，根因尚未确认；本发行版尚未通过正式 30 分钟稳定性验收。

### 其他边界

完整数据库平台实库矩阵、通用 Task 网络调度和任意动态堆组合不在当前完整支持范围内。任务数、连接数和 HTTP transport buffer 的限制各有作用域，不能作为整个进程 RSS 的统一上限。

## 进程隔离与内存限制

| 平台    | `ku supervise` 条件与指标                                                                                                                                                                                               |
| ------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Linux   | 需要可写且已委托的 cgroup v2 domain 根目录，启用 `memory`、`pids`，提供 `memory.max`、`memory.swap.max`、`memory.oom.group`、`pids.max`、`cgroup.kill` 等接口。按内核 cgroup 计费内存限制；缺少条件时在应用执行前拒绝。 |
| Windows | 嵌套 Job Object 限制单 worker 与 worker 合计的提交虚拟内存，指标不是 RSS；应用初始线程保持暂停，核对初始提交量与 Job 配置后才放行。宿主策略可能拒绝 Job 分配。                                                          |
| macOS   | 原生不支持此隔离模式，在应用启动前明确拒绝；需要时可使用满足条件的 Linux 主机或 VM，不提供自动 VM 桥接。                                                                                                                |

worker 资源域覆盖适用的 Ku、MySQL、libpq、TLS 等第三方库分配，但 Linux 计费内存与 Windows 提交量不是同一指标。Linux `memory.max` 允许特定情况下瞬时越限，不能承诺任意时刻所有物理内存绝不超过一个固定字节数。

协调器自身没有 OS 内存硬限制；控制预留和有界准入不是 RSS 硬限，单 Task 完整堆未精确计量。共享连接池、缓存与外部库内存不能精确分摊到每个 Task。外部数据库、宿主/VM 与构建编译器不在 worker 限额内，不宣称整个系统内存绝对封顶。

OS 终止超限 worker 后，协调器报告失败，确认回收后才归还预算；部分分配可提前拒绝，但第三方库耗尽并不保证转成普通 Result。**数据库不自动重试**，worker 退出不证明数据库回滚，副作用不确定时如实报告。

详见[受控程序说明](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/process-execution.md)、[并发说明](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/concurrency.md)。

## 开发时运行受控程序

先准备依赖和普通 `hello.ku`。Linux 的 `KU_CGROUP_ROOT` 必须由管理员或服务管理器提供真实可委托根，不能用普通目录代替：

```sh
ku supervise --worker-memory-bytes 134217728 --total-memory-bytes 536870912 \
  --max-workers 2 --max-queued-jobs 32 --max-queued-bytes 262144 \
  --max-output-bytes 1048576 --deadline-ms 30000 \
  --cgroup-root "$KU_CGROUP_ROOT" -- ./hello.ku
```

Windows 沿用数值参数，省略 `--cgroup-root`。字节数和毫秒数用十进制数字；`--` 后每个入口是完整程序，不是请求路由或通用网络调度接口。命令依次输出 configuration、execution、summary JSONL，任一程序失败则最终非零退出。native 程序先构建，再通过 `--native` 提交；编译器不在 worker 限制内。
