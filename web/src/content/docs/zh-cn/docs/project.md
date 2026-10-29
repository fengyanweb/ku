---
title: 项目与贡献
description: Ku 项目状态、公开源码、许可边界与提交问题的方式。
kuVersion: '0.0.19'
verifiedAt: '2026-10-28'
sources:
  [
    'README.md',
    'Cargo.toml',
    'LICENSE',
    'editors/vscode-ku/package.json',
    'docs/v0.0.19.md',
    'docs/syntax.md',
  ]
---

项目名称为 **Ku 编程语言 / Ku programming language**，CLI 名称为 `ku`。Ku 是具有显式所有权与 Result 错误处理的编程语言与工具链，提供解释运行、可执行 runner 和已支持程序的原生 C 编译路径。当前版本为 Ku 0.0.19。

<span id="license"></span>

## 源码与许可

Ku 与 VS Code 插件采用 **Apache-2.0** 许可证。可查看[源码](https://github.com/fengyanweb/ku)、[0.0.19 发行包](https://github.com/fengyanweb/ku/releases/tag/v0.0.19)和[许可证全文](https://github.com/fengyanweb/ku/blob/v0.0.19/LICENSE)。

第三方组件仍遵循各自许可证；发行包内保留了相应许可证与声明。

## 提交问题

通过[仓库 Issues](https://github.com/fengyanweb/ku/issues)提交可复现问题。报告尽量包含：

邮件反馈: [kagura.gc@qq.com](mailto:kagura.gc@qq.com)

- Ku 版本、Git commit（使用开发源码时）和目标系统 / 架构。
- 完整命令、后端、最小 `.ku` 源码。
- 完整诊断或结构化 Error、预期与实际输出。
- 涉及构建时的 Rust / C 工具链版本，以及是否存在本地 import。

请勿提交 token、密码、真实连接字符串或含敏感业务数据的日志。性能问题应同时记录环境、负载、测量方法和原始结果；一次样例成功不能代表生产吞吐。

## 贡献前先读

[语法参考](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/syntax.md)与[语言语义](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/semantics.md)规定语言行为。选择修改目标时，请区分已发布行为与开发分支。

修改应尽量小，说明复现、原因、影响和验证范围。源码、文档与示例应使用同一版本；维护者审核后再合并贡献。

本站暂不提供在线 Ku 解释器。

继续阅读[快速开始](/zh-cn/docs/getting-started/)、[支持范围](/zh-cn/docs/support/)与[发布记录](/zh-cn/releases/)。
