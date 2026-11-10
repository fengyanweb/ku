---
title: VS Code
description: 安装 Ku 插件，配置运行、检查、格式化和代码补全。
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'editors/vscode-ku/package.json',
    'editors/vscode-ku/README.md',
    'editors/vscode-ku/src/language.ts',
    'editors/vscode-ku/src/completionModel.ts',
    '.github/scripts/export-release.py',
  ]
slug: zh-cn/v0.0.18/docs/tools/vscode
---

Ku 插件为 VS Code 1.85 及以上版本提供语法高亮、代码片段、补全、格式化和编译器诊断。请同时[安装 Ku CLI](/zh-cn/v0.0.18/docs/install/)，插件通过 CLI 检查和运行程序。

## 安装插件

从 [0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) 下载对应系统的压缩包并解压。每个平台包都包含 `ku-language-0.0.18.vsix` 和 CLI。VSIX 位于压缩包内，没有单独的 Release 下载文件。

在 VS Code 中打开**扩展 → … → 从 VSIX 安装…**，选择该文件，然后执行 **Developer: Reload Window**。也可以在解压目录执行：

```sh
code --install-extension ./ku-language-0.0.18.vsix --force
```

这种方式直接安装已发布的 VSIX，不依赖 Marketplace 上架。

## 指定 Ku 程序

将 `ku` 所在目录加入 `PATH`，或者将 **Ku: Executable Path** 设置为程序的完整路径。例如 macOS：

```json
{
  "ku.executablePath": "/Users/you/tools/ku/ku"
}
```

Windows 请指向 `ku.exe`。保留解压包中其他文件的相对位置。执行 **Ku: Show Version**，可以检查插件找到的 CLI 版本；状态栏也会比较 CLI 与插件版本。

## 检查与运行

将以下程序保存为 `hello.ku`：

```ku
fn main() {
    println("Hello, Ku!")
}
```

先执行 **Ku: Check Current File**，再执行 **Ku Run**。编译器错误显示在**问题**面板中，命令输出显示在 Ku 输出频道或终端。默认在打开和保存文件时检查；需要编辑时自动检查，可开启 `ku.checkOnChange`。

| 命令                   | 用途                                |
| ---------------------- | ----------------------------------- |
| Ku Run                 | 运行包含 `main()` 的当前 `.ku` 文件 |
| Ku: Check Current File | 检查语法与类型                      |
| Ku: Show IR            | 查看中间表示                        |
| Ku: Build Current File | 使用 CLI 默认构建路径编译           |
| Ku: Build Native C     | 生成 C 源码（不链接可执行文件）     |
| Ku: Package GC         | 执行 CLI 的包缓存清理命令           |
| Ku: Show Version       | 查看实际找到的 CLI 版本             |

可以通过**格式化文档**整理代码，或启用 `editor.formatOnSave`。插件也支持 `ku.mod` / `ku.lock` 高亮、本地和导入定义跳转、大纲、导入路径补全，以及部分诊断的快速修复。

## 补全与示例

输入 `http.` 查看 HTTP 辅助函数，`time.` 查看日期和时钟操作，`json.` 查看编码与解码。HTTP 构造器会插入 `http.service()` / `http.server()`；`http.server` 代码片段可以生成完整服务程序。API 与可运行示例见 [HTTP 参考](/zh-cn/v0.0.18/docs/stdlib/http/)。

补全来自插件内置 API 列表和部分接收者名称规则，尚不能推断每个变量的类型，也不是完整的编译器语言服务器。补全出现某个方法，不代表所有后端都支持它；执行条件以对应库文档为准。

## 更新

安装与你使用的 Ku 版本对应的 VSIX，并重新加载 VS Code。公开的 v0.0.18 压缩包包含该版发布时的插件。仓库后续对补全、悬浮说明的修改，需要重新构建和安装插件才会生效，不会自动改变已经下载的 VSIX。

需要生成可执行文件时，在终端运行 `ku build --backend c main.ku -o app`；Windows 使用 `app.exe`。
