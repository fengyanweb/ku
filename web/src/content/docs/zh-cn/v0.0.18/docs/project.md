---
title: 项目与贡献
description: Ku 项目状态、公开源码、许可边界与提交问题的方式。
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'README.md',
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/v0.0.18.md',
    'docs/syntax.md',
  ]
slug: zh-cn/v0.0.18/docs/project
---

项目名称为 **Ku 编程语言 / Ku programming language**，CLI 名称为 `ku`。Ku 是具有显式所有权与 Result 错误处理的编程语言与工具链，提供解释运行、可执行 runner 和已支持程序的原生 C 编译路径。当前版本为 Ku 0.0.18。

<span id="license"></span>

## 源码与许可

[查看 GitHub 源码](https://github.com/fengyanweb/ku)与[发行页面](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1)。源码公开可查看，但 Ku 自身代码**未授予开源许可证**，编辑器扩展标记为 `UNLICENSED`。公开源码和二进制发布不自动授予开源许可。

发行归档中的第三方许可只适用于相应第三方组件，不能据此推断 Ku 本身的使用、修改或再分发授权。使用、修改和再分发所需的权限，应以项目维护方明确提供的授权为准。

## 提交问题

通过[仓库 Issues](https://github.com/fengyanweb/ku/issues)提交可复现问题。报告尽量包含：

- Ku 版本、Git commit（使用开发源码时）和目标系统 / 架构。
- 完整命令、后端、最小 `.ku` 源码。
- 完整诊断或结构化 Error、预期与实际输出。
- 涉及构建时的 Rust / C 工具链版本，以及是否存在本地 import。

请勿提交 token、密码、真实连接字符串或含敏感业务数据的日志。性能问题应同时记录环境、负载、测量方法和原始结果；一次样例成功不能代表生产吞吐。

## 贡献前先读

[语法参考](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/syntax.md)与[语言语义](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/semantics.md)规定语言行为。选择修改目标时，请区分已发布行为与开发分支。

修改应尽量小，说明复现、原因、影响和验证范围。提交内容应与对应版本的源码和示例一致；贡献接受流程与许可安排由维护方决定。

本站暂不提供在线 Ku 解释器。

继续阅读[快速开始](/zh-cn/v0.0.18/docs/getting-started/)、[支持范围](/zh-cn/v0.0.18/docs/support/)与[发布记录](/zh-cn/v0.0.18/releases/)。
