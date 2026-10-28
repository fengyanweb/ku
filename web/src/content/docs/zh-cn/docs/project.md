---
title: 项目与贡献
description: Ku 项目状态、公开源码、许可边界与提交问题的方式。
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-28'
sources:
  [
    'README.md',
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/v0.0.19.md',
    'docs/syntax.md',
    'LICENSE',
    'NOTICE',
  ]
---

项目名称为 **Ku 编程语言 / Ku programming language**，CLI 名称为 `ku`。Ku 是仍在开发中的小型编程语言与解释器，已发布版本及平台资产以发行页面为准。

## 源码与许可

[查看 GitHub 源码](https://github.com/fengyanweb/ku)与[发行页面](https://github.com/fengyanweb/ku/releases)。Ku 使用 [Apache License 2.0](https://github.com/fengyanweb/ku/blob/main/LICENSE)。请阅读仓库 [NOTICE](https://github.com/fengyanweb/ku/blob/main/NOTICE)，并以所用发行版或源码副本中附带的许可文本为准。

第三方组件保留各自的版权与许可条款，其通知和许可材料与 Ku 自身许可分别适用；再分发时应保留对应组件要求的材料。

## 提交问题

通过[仓库 Issues](https://github.com/fengyanweb/ku/issues)提交可复现问题。报告尽量包含：

- Ku 版本、Git commit（使用开发源码时）和目标系统 / 架构。
- 完整命令、后端、最小 `.ku` 源码。
- 完整诊断或结构化 Error、预期与实际输出。
- 涉及构建时的 Rust / C 工具链版本，以及是否存在本地 import。

请勿提交 token、密码、真实连接字符串或含敏感业务数据的日志。性能问题应同时记录环境、负载、测量方法和原始结果；一次样例成功不能代表生产吞吐。

## 贡献前先读

[语法合同](https://github.com/fengyanweb/ku/blob/main/docs/syntax.md)与[语义合同](https://github.com/fengyanweb/ku/blob/main/docs/semantics.md)规定语言行为。CI、实现和规范需一起核对，不能把工作日志目标当成已实现功能。

修改应尽量小，说明复现、原因、影响和验证范围。网站内容与对应源码一起审查，接受贡献的具体流程仍由维护方决定，本站不另行虚构贡献协议。

当前官网是静态站点，正文、导航和代码无需业务后端。站内搜索在浏览器加载 Pagefind 索引；不提供线上 Ku 代码执行服务。

继续阅读[快速开始](/zh-cn/docs/getting-started/)、[支持范围](/zh-cn/docs/support/)与[发布记录](/zh-cn/releases/)。
