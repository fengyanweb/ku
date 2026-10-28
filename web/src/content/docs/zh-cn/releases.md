---
title: '版本与发布记录'
description: '版本与发布记录'
kuVersion: '0.0.19'
verifiedAt: '2026-10-21'
sources: ['docs/history.md', 'docs/v0.0.17.md', 'docs/v0.0.19.md']
---

## 0.0.19

Ku 0.0.19 是供受控环境试用的实验预发行版，CLI 与 VS Code 插件版本一致。它汇集已实现的 native Task、HTTP、PostgreSQL 和服务管理工作，保留旧 JSON 默认实现，采用 Apache-2.0。完整 M:N、通用 I/O 覆盖和数据库完整平台矩阵尚未完成；历史 HTTP 504 与取消场景长尾仍按已知问题记录，不宣称长期或生产验收已通过。

[下载与校验](https://github.com/fengyanweb/ku/releases/tag/v0.0.19) · [安装](/zh-cn/docs/install/) · [完整说明](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/v0.0.19.md) · [已知问题](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/v0.0.19-known-issues.md)

实际公开状态、附件与校验信息以 Release 页面为准。升级后应重新构建 native 程序；本版安装包目标仍为 macOS ARM64、Linux x64 和 Windows x64。

## 0.0.18 修订版 1（历史）

历史修订版保留 0.0.18 版本号，发布标识为 `v0.0.18-r1`。新增调用链、大浮点源码字面量、`time.date(Time)` 和插件修复；[初版 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18) 保留供追溯。

该版为实验预发行版，包含有限 Task/等待与关闭修复、HTTP 连接复用与缓冲限制、JSON 大浮点往返修复，以及显式 `ku supervise` 受控 worker。

[下载与校验](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) · [固定文档](/zh-cn/v0.0.18/) · [已知限制](/zh-cn/docs/support/)

**HTTP 已知问题：历史出现过 504，本轮短测未复现，根因未确认，正式 30 分钟验收尚未通过。** 完整 M:N、通用网络调度和数据库完整平台矩阵尚未完成。

## 0.0.17

同步只读借用、自举 Parser 的花括号 while 与跨平台工具修复。

[下载与发布说明](https://github.com/fengyanweb/ku/releases/tag/v0.0.17) · [固定文档](/zh-cn/v0.0.17/)

## 早期开发记录

以下 0.0.1–0.0.16 记录早于新版官网。链接固定到 0.0.17 标签中保存的历史文件；它们是开发记录，不表示每个版本都有 GitHub Release 或可下载安装包。旧语法与当前版本可能不同。

| 版本   | 记录内容                                              | 原始记录                                                                |
| ------ | ----------------------------------------------------- | ----------------------------------------------------------------------- |
| 0.0.16 | native 标准库、数据库 client、registry 与 target 发布 | [0.0.16](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.16.md) |
| 0.0.15 | 对象解构、HTTP 调用规则与示例                         | [0.0.15](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.15.md) |
| 0.0.14 | 项目模板、HTTP 状态 helper 与匿名 handler             | [0.0.14](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.14.md) |
| 0.0.13 | 统一 build、项目入口、时间 API 与编辑器同步           | [0.0.13](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.13.md) |
| 0.0.12 | HTTP Response、连接复用、文件写入与严格 bool          | [0.0.12](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.12.md) |
| 0.0.11 | Result ABI、finally CFG 与 file 依赖缓存              | [0.0.11](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.11.md) |
| 0.0.10 | 运行时捕获、Result CFG 与 native 控制流               | [0.0.10](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.10.md) |
| 0.0.9  | typed temp IR、锁文件与 native C 原型                 | [0.0.9](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.9.md)   |
| 0.0.8  | 引用捕获闭包前置与 typed CFG IR                       | [0.0.8](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.8.md)   |
| 0.0.7  | 可恢复集合读取、包边界与 IR                           | [0.0.7](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.7.md)   |
| 0.0.6  | 标准库与编译器模块拆分                                | [0.0.6](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.6.md)   |
| 0.0.5  | Result、try/catch/finally 与 ?                        | [0.0.5](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.5.md)   |
| 0.0.4  | 闭包、match 与复合值修改                              | [0.0.4](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.4.md)   |
| 0.0.3  | 数组、结构体、enum、对象与模块导入                    | [0.0.3](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.3.md)   |
| 0.0.2  | 函数值、箭头函数、模板字符串与 for                    | [0.0.2](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.2.md)   |
| 0.0.1  | 基础解释器、函数、条件与循环                          | [0.0.1](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/v0.0.1.md)   |
