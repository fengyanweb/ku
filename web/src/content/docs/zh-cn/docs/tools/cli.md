---
title: '命令行工具'
description: 'Ku 检查、运行、构建、项目与包命令。'
kuVersion: '0.0.18'
verifiedAt: '2026-10-21'
sources: ['src/cli.rs', 'docs/package.md', 'docs/process-execution.md']
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
