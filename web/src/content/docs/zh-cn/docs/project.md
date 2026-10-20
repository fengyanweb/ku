---
title: 项目与贡献
description: Ku 项目状态、公开源码、许可边界与提交问题的方式。
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'README.md',
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/v0.0.18.md',
    'docs/syntax.md',
  ]
---

本文档对应 v0.0.18 实验预发行版的冻结范围；实际公开状态与下载资产以 GitHub Release 为准。项目名称为 **Ku 编程语言 / Ku programming language**，CLI 名称为 `ku`。Ku 是仍在开发中的小型编程语言与解释器，当前公开发行状态为 v0.0.18 实验预发行版。

## 源码与许可

[查看 GitHub 源码](https://github.com/fengyanweb/ku)与[发行页面](https://github.com/fengyanweb/ku/releases/tag/v0.0.18)。源码公开可查看，但 Ku 自身代码**未授予开源许可证**，编辑器扩展标记为 `UNLICENSED`。公开源码和二进制发布不自动授予开源许可。

发行归档中的第三方许可只适用于相应第三方组件，不能据此推断 Ku 本身的使用、修改或再分发授权。需要相关授权时应由项目维护方明确说明；本站不添加新的许可承诺。

## 提交问题

通过[仓库 Issues](https://github.com/fengyanweb/ku/issues)提交可复现问题。报告尽量包含：

- Ku 版本、Git commit（使用开发源码时）和目标系统 / 架构。
- 完整命令、后端、最小 `.ku` 源码。
- 完整诊断或结构化 Error、预期与实际输出。
- 涉及构建时的 Rust / C 工具链版本，以及是否存在本地 import。

请勿提交 token、密码、真实连接字符串或含敏感业务数据的日志。性能问题应同时记录环境、负载、测量方法和原始结果；一次样例成功不能代表生产吞吐。

## 贡献前先读

[语法合同](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/syntax.md)与[语义合同](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/semantics.md)规定语言行为。CI、实现和规范需一起核对，不能把工作日志目标当成已实现功能。

修改应尽量小，说明复现、原因、影响和验证范围。网站内容与对应源码一起审查，已有测试源码、工作区改动和独有证据需要保留。接受贡献的具体流程与许可安排仍由维护方决定，本站不虚构贡献协议。

当前官网是静态站点，正文、导航和代码无需业务后端。站内搜索在浏览器加载 Pagefind 索引；不提供线上 Ku 代码执行服务。

继续阅读[快速开始](/zh-cn/docs/getting-started/)、[支持范围](/zh-cn/docs/support/)与[发布记录](/zh-cn/releases/)。
