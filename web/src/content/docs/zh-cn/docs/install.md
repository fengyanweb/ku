---
title: 安装与下载
description: 下载 Ku 0.0.18 实验预发行包，了解平台、构建工具与安装验证。
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-21'
sources:
  [
    'Cargo.toml',
    'docs/v0.0.18.md',
    'docs/history.md',
    '.github/scripts/export-release.py',
  ]
---

当前下载为 **Ku 0.0.18 修订版 1**（发布标识 `v0.0.18-r1`），CLI 和插件版本仍显示 `0.0.18`。选择与你的系统和 CPU 匹配的安装包。

## 选择对应平台

从 [v0.0.18-r1 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) 下载以下目标归档、并列 manifest 与 `SHA256SUMS`。`native_ci-*` 是 CI 样例，不是 CLI 安装包。

| 系统    | 架构                  | 归档文件                                                                                                                                                 |
| ------- | --------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| macOS   | Apple Silicon / arm64 | [`ku-v0.0.18-aarch64-apple-darwin.tar.gz`](https://github.com/fengyanweb/ku/releases/download/v0.0.18-r1/ku-v0.0.18-aarch64-apple-darwin.tar.gz)         |
| Windows | x64 / MSVC            | [`ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`](https://github.com/fengyanweb/ku/releases/download/v0.0.18-r1/ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz)     |
| Linux   | x64 / GNU             | [`ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz`](https://github.com/fengyanweb/ku/releases/download/v0.0.18-r1/ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz) |

[下载校验文件: SHA256SUMS](https://github.com/fengyanweb/ku/releases/download/v0.0.18-r1/SHA256SUMS)

没有核实的其他平台安装包。跨平台编译能力也不表示任意操作系统或架构已有发行包。

## 解压与验证

1. 对照 Release 的 `SHA256SUMS` 校验归档；并列 `.manifest.json` 提供归档和文件清单信息。校验和用于检查完整性，不是发布者签名。
2. 把**完整归档**解压到一个空目录，阅读其中的 `README-INSTALL.md`。保留 `ku`、`libku.rlib`、`deps/` 和 `native-tls/` 的相对布局。
3. 在解压目录检查版本。macOS / Linux：

```sh
./ku --version
```

Windows PowerShell：

```powershell
.\ku.exe --version
```

应显示版本 `0.0.18`。需要从任意目录调用时，把该目录加入 `PATH`，再执行 `ku --version`。VS Code 扩展通过归档内的 `ku-language-0.0.18.vsix` 安装。

## 解释器与 VS Code 插件

上面的平台包同时包含 Ku CLI（`ku run` 使用解释器）和 `ku-language-0.0.18.vsix`。插件当前随平台包下载，没有单独的 VSIX 下载资产；这里也没有浏览器内运行解释器。

解压后，在 VS Code 的扩展面板选择 **… → 从 VSIX 安装…**，打开 `ku-language-0.0.18.vsix`，再重新加载窗口。也可以在解压目录运行：

```sh
code --install-extension ./ku-language-0.0.18.vsix --force
```

插件需要找到 Ku CLI。把解压目录加入 `PATH`，或在 VS Code 设置中把 `ku.executablePath` 指向 `ku`（Windows 为 `ku.exe`）的完整路径。

## 运行与构建需要什么

| 操作                                    | 额外要求                                                       |
| --------------------------------------- | -------------------------------------------------------------- |
| `ku run hello.ku` / `ku check hello.ku` | 对应平台的 Ku CLI                                              |
| 默认 `ku build hello.ku`                | 匹配目标的 Rust **1.89.0** 及系统 linker；保留随包的 Rust 依赖 |
| `ku build --backend c hello.ku`         | 可工作的目标 C 编译器与 linker                                 |
| native 数据库程序                       | 另外安装对应数据库客户端动态库；发行包未包含这些库             |

默认 build 的 runner 只嵌入入口源码，导入模块仍从原路径读取。完整本地 import graph 的源码无关部署需要 native C 构建，并受[后端支持范围](/zh-cn/docs/support/)约束。

## 从源码试用开发分支

在已取得的仓库根目录执行：

```sh
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

CLI 与 Cargo 版本为 `0.0.18`。从源码复现时记录 Git commit、平台与工具链；源码构建成功不等于安装包已经发布。

## 卸载

归档式安装可移除自行解压的 Ku 目录，撤销自己添加的 `PATH` 项，并在 VS Code 中卸载扩展。删除前保留该目录里自行创建的源码和配置。当前未核实独立的一键安装器、包管理器安装入口或 `ku uninstall` 命令。

下一步：[运行第一个程序](/zh-cn/docs/getting-started/)。安装合同来源：[发布归档说明](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/history.md)。

`ku supervise` 的系统条件与内存边界见[支持范围](/zh-cn/docs/support/)。macOS CLI 可运行程序，但原生不支持该隔离模式。
