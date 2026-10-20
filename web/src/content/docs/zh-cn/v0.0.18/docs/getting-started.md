---
title: 快速开始
description: 用 Ku 编写 Hello Ku，检查源码并了解解释运行与构建的区别。
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  - examples/hello.ku
  - README.md
  - docs/syntax.md
  - docs/v0.0.18.md
slug: zh-cn/v0.0.18/docs/getting-started
---

本文档对应 v0.0.18 实验预发行版的冻结范围；实际公开状态与下载资产以 GitHub Release 为准。本页使用 v0.0.17 已有的基础语法与 CLI。先完成[安装](/zh-cn/v0.0.18/docs/install/)，并确认 `ku --version` 可执行。

## 写下第一个程序

新建 `hello.ku`，内容与仓库 `examples/hello.ku` 一致：

```ku
fn main() {
    println("Hello Ku")
}
```

`fn main()` 是入口，`println` 输出一行文本。Ku 源码不使用 `let` 或 `let mut`。

## 检查并运行

在文件所在目录执行：

```sh
ku check hello.ku
ku run hello.ku
```

`check` 检查词法、语法和基础语义，不运行程序。正常运行的预期标准输出为：

```text
Hello Ku
```

如果没有把 Ku 加入 `PATH`，请使用安装目录里 `ku` 的完整路径。Windows 可执行文件名为 `ku.exe`。

## 变量与函数

下面是基础语法片段，放进 `main` 可以读取并输出变量：

```ku
name = "Ku"
count:int = 3
println(name)
println(count)
```

首次赋值声明变量；需要显式类型时写 `name:type = value`。`print` 不自动换行，`println` 会换行。

## 构建可执行文件

安装页列出了构建工具前置要求。默认构建：

```sh
ku build hello.ku
```

默认后端生成解释器打包型可执行文件。native C 后端需要单独的 C 编译器：

```sh
ku build --backend c hello.ku
```

默认输出位于 `.ku/build/<profile>/`；以实际 CLI 输出路径为准。两种构建方式的覆盖范围不同，不能把 `ku check` 成功当作 native 编译成功。

注意：`ku build --native hello.ku` **不带 `-o` 时只生成 C 源码**，不执行链接。首个项目建议使用上面的 `--backend c` 形式。

## 创建项目

需要包目录时，可使用内置模板：

```sh
ku create HelloWorld --template basic
cd HelloWorld
ku check
ku run
```

包通过 `ku.mod` 指定入口。`ku create --list` 查看真实模板列表；当前没有 `ku test` 或 `ku fmt` 命令。

继续阅读[语言指南](/zh-cn/v0.0.18/docs/guide/language/)、[错误与排查](/zh-cn/v0.0.18/docs/guide/errors/)和[后端支持](/zh-cn/v0.0.18/docs/support/)。本页的输出是示例预期，不代替你所在平台的安装验证。
