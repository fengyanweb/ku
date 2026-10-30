---
title: '版本与发布记录'
description: '版本与发布记录'
kuVersion: '0.0.19'
verifiedAt: '2026-10-29'
sources: ['docs/history.md', 'docs/v0.0.17.md', 'docs/v0.0.18.md']
---

## 开发中的 CLI 修复候选

正在准备独立的 Runner 工具链/依赖身份修复、新增 `ku doctor` 与统一子命令帮助；已有简写补充见[CLI 参考](/zh-cn/docs/tools/cli/)。**尚未发布，下面的 0.0.19 下载仍是原发行资产。** 该修复不覆盖旧标签、不自动采用性能开发候选；具体错误及候选处理见[Runner 排查](/zh-cn/docs/guide/errors/#runner-build)。

## 0.0.19

Ku 0.0.19 是供受控环境试用的实验预发行版，CLI 与 VS Code 插件版本一致。它汇集已实现的 native Task、HTTP、PostgreSQL 和服务管理工作，保留旧 JSON 默认实现，采用 Apache-2.0。完整 M:N、通用 I/O 覆盖和数据库完整平台矩阵尚未完成；历史 HTTP 504 与取消场景长尾仍按已知问题记录，不宣称长期或生产验收已通过。

[下载与校验](https://github.com/fengyanweb/ku/releases/tag/v0.0.19) · [安装](/zh-cn/docs/install/) · [完整说明](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/v0.0.19.md) · [已知问题](/zh-cn/docs/support/#已知问题)

实际公开状态、附件与校验信息以 Release 页面为准。升级后应重新构建 native 程序；本版安装包目标仍为 macOS ARM64、Linux x64 和 Windows x64。

## 0.0.18

Ku 0.0.18 为实验版本，包含受限 native Task、计时器与后台 I/O、HTTP/1.1 连接复用、JSON 编解码，以及通过 `ku supervise` 显式管理完整程序的资源限制。

本版已包含 native C 浮点常量与大浮点构建、链式调用接收者重复求值、`time.date(Time)` 的 Result 检查，以及 VS Code 插件版本读取和补全修复。升级后请重新编译受影响的 native 程序；即使扩展版本号相同，旧的 `ku-language-0.0.18.vsix` 也需要重新安装。

[安装 Ku](/zh-cn/v0.0.18/docs/install/) · [下载 0.0.18](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) · [支持平台与已知问题](/zh-cn/v0.0.18/docs/support/)

## v0.0.17

同步只读借用、自举 Parser 的花括号 while 与跨平台工具修复。对应[发布记录](https://github.com/fengyanweb/ku/releases/tag/v0.0.17)，旧 API 与当前版本可能不同。

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
