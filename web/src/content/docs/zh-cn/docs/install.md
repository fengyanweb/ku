---
title: 安装与下载
description: 下载 Ku 0.0.19 实验预发行包，了解平台、构建工具与安装验证。
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-28'
sources:
  [
    'Cargo.toml',
    'docs/v0.0.19.md',
    'docs/history.md',
    '.github/scripts/export-release.py',
  ]
---

下载适合你电脑的 **Ku 0.0.19**。当前为实验预发行版，平台包已包含命令行工具、解释器和 VS Code 插件。

## 1. 下载 Ku

| 系统    | 架构                  | 下载                                                                                                                          | 大小    |
| ------- | --------------------- | ----------------------------------------------------------------------------------------------------------------------------- | ------- |
| macOS   | Apple Silicon / ARM64 | [下载 macOS (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-aarch64-apple-darwin.tar.gz)     | 39.8 MB |
| Windows | x64                   | [下载 Windows (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-x86_64-pc-windows-msvc.tar.gz) | 44.1 MB |
| Linux   | x64 / GNU             | [下载 Linux (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-x86_64-unknown-linux-gnu.tar.gz) | 48.3 MB |

## 2. 解压并运行

将**整个安装包**解压到一个空文件夹，例如用户目录下的 `ku` 文件夹。保留解压后的全部文件，不要只移动可执行文件。在该文件夹打开终端。

macOS / Linux：

```sh
./ku --version
```

Windows PowerShell：

```powershell
.\ku.exe --version
```

显示 `ku 0.0.19` 即可。现在就能运行和检查 `.ku` 文件，这两项操作不需要安装 Rust。

<details>
<summary>可选：在任意目录使用 ku</summary>

如果解压到了 `~/ku`，在所用 shell 的启动文件（如 `~/.zshrc` 或 `~/.bashrc`）中加入这一行：

```sh
export PATH="$HOME/ku:$PATH"
```

Windows 打开“编辑账户的环境变量”，编辑用户 `Path`，新增解压文件夹的完整路径，例如 `%USERPROFILE%\ku`。保留原有条目。

重新打开终端，运行 `ku --version` 确认。

</details>

## 3. 安装 VS Code 插件

在 VS Code 扩展面板选择 **… → 从 VSIX 安装…**，选择解压目录里的 `ku-language-0.0.19.vsix`，然后重新加载窗口。

如果没有配置 `PATH`，将设置项 `ku.executablePath` 指向 `ku`（Windows 为 `ku.exe`）的完整路径。

<details>
<summary>校验下载文件</summary>

将安装包的 SHA-256 与 [SHA256SUMS](https://github.com/fengyanweb/ku/releases/download/v0.0.19/SHA256SUMS) 对照。[发布页](https://github.com/fengyanweb/ku/releases/tag/v0.0.19)还为每个平台提供 `.manifest.json` 文件清单与校验值。包内 `README-INSTALL.md` 提供进一步说明。

</details>

<details>
<summary>编译自己的 Ku 程序</summary>

`ku run` 和 `ku check` 只需对应平台的 CLI。默认 `ku build` 另需 Rust **1.89.0** 和系统链接器；`ku build --backend c` 另需目标 C 编译器和链接器。native 数据库程序还需安装匹配的数据库客户端库，这些库不包含在平台包内。

</details>

<details>
<summary>卸载</summary>

先保留自行创建的源码与配置，再删除解压文件夹、自己添加的 `PATH` 条目，并在 VS Code 中卸载插件。

</details>

[下一步：运行第一个程序](/zh-cn/docs/getting-started/)
