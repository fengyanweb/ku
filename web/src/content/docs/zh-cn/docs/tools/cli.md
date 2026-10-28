---
title: '命令行工具'
description: 'Ku 检查、运行、构建、项目与包命令。'
kuVersion: '0.0.19'
verifiedAt: '2026-10-28'
sources:
  [
    'src/cli.rs',
    'docs/package.md',
    'docs/process-execution.md',
    'src/service_supervision.rs',
    'src/backend/c_http_runtime.c',
  ]
---

本页列出日常开发命令。先运行 `ku --version` 确认版本，`ku --help` 查看实际选项。

| 命令                                  | 用途                                   |
| ------------------------------------- | -------------------------------------- |
| `ku check main.ku`                    | 检查源码，不执行                       |
| `ku check --json main.ku`             | 输出编辑器可读的 JSON Lines 诊断       |
| `ku run main.ku`                      | 解释执行                               |
| `ku build main.ku`                    | 构建默认 runner                        |
| `ku build --backend c main.ku -o app` | 编译并链接 native C 可执行文件         |
| `ku build --native main.ku`           | 只发射 C 源码，不链接                  |
| `ku ir main.ku`                       | 查看优化后的 IR                        |
| `ku llvm main.ku`                     | 输出实验性 LLVM 文本，不生成可执行文件 |
| `ku create Hello --template basic`    | 新建项目                               |
| `ku init --template basic`            | 初始化当前目录                         |
| `ku template list`                    | 列出模板                               |
| `ku package resolve --locked`         | 按项目 lock 固定依赖                   |
| `ku package gc`                       | 清理未使用的包缓存                     |

## 选择执行方式

`check` 与 `run` 不要求 Rust/C 编译器。默认 `build` 需要 Rust 1.89.0、linker 和安装包完整的 `libku.rlib` / `deps`；runner 只嵌入入口文件，运行含本地导入的程序时仍需原模块源码。native C 需要 C 编译器，编译展开导入图，所得程序不依赖原 Ku 源码。

Windows 输出名建议为 `app.exe`。具体命令参数及平台见[安装](/zh-cn/docs/install/)和[支持矩阵](/zh-cn/docs/support/)。当前没有 `ku fmt`、`ku test` 或 `ku doc` 命令；VS Code 提供的格式化属于插件功能。

## 受控执行

`ku supervise` 是显式的完整程序 worker 管理模式，不会自动应用到普通 `run` 或 HTTP 请求。Linux 需要已委托的 cgroup v2，Windows 使用提交内存限制；macOS 原生不支持该隔离模式。配置、失败语义与未覆盖范围见[平台与进程限制](/zh-cn/docs/support/)。

## 常驻 native HTTP 服务

`ku service --help` 说明独立的常驻服务监督器。先构建 [native HTTP 示例](/zh-cn/docs/stdlib/http/)。目标程序必须支持 native 认证健康/排空协议；probe 与 control 地址须为不同、非零且端口空闲的 loopback 地址。

运行前准备私有普通文件 `service.token`，内容为 32–128 个 ASCII 字母或数字；Unix 下只能所有者访问（`chmod 600 service.token`）。使用自行生成的不可预测 token，不把 token 随源码提交，再执行：

```sh
ku service --probe-addr 127.0.0.1:18181 \
  --control-addr 127.0.0.1:19081 --control-token-file service.token \
  --startup-timeout-ms 5000 --probe-timeout-ms 250 \
  --probe-interval-ms 1000 --failure-threshold 3 \
  --drain-timeout-ms 10000 --restart-limit 5 --restart-window-ms 60000 \
  --backoff-ms 100 --max-backoff-ms 5000 -- ./native-server
```

上述数值显式给出了监督器默认值。时长单位为毫秒，不能为 0。`--failure-threshold` 为 1–100，`--restart-limit` 为 1–1000；backoff 不能超过 max-backoff，probe-timeout 不能超过 startup-timeout。`--` 后接可执行文件及参数，Windows 改为实际 `.exe` 路径。

control listener 通过 `X-Ku-Control-Token` 认证暴露 `GET /status`、`GET /metrics`、`POST /stop`。它管理单个服务实例的健康、准入/排空与有界重启，不是请求代理或数据库重试机制。此模式将被管理程序 stdout/stderr 接到 null device，不保留应用日志，应按控制指标观察状态。

不带内存选项时普通模式不强制 OS 内存上限。显式内存模式必须同时指定 `--worker-memory-bytes` 与 `--total-memory-bytes`；Linux 还需委派的 `--cgroup-root`，先阅读[平台限制](/zh-cn/docs/support/)。native macOS 拒绝此硬限制模式，但支持普通监督。指标区分当前状态与失败；健康探测成功不证明业务 SLO 或已有稳定生产后端。
