---
title: '示例集'
description: '按任务查找可以运行的 Ku 示例。'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources: ['examples/README.md', 'examples/http_server.ku', 'examples/json.ku']
slug: zh-cn/v0.0.18/docs/examples
---

这些示例按任务组织。每个链接包含完整程序、运行方式以及后端要求。需要输入文件、服务器或数据库时，页面会明确列出准备步骤。

| 任务            | 示例与运行环境                                                                |
| --------------- | ----------------------------------------------------------------------------- |
| 第一个程序      | [Hello Ku](/zh-cn/v0.0.18/docs/getting-started/)；解释器或 native C                   |
| 中文与 emoji    | [字符串](/zh-cn/v0.0.18/docs/stdlib/string/)；解释器或已列出的 native 方法            |
| JSON 往返和错误 | [JSON](/zh-cn/v0.0.18/docs/stdlib/json/)；解释器或同步 native C                       |
| 文件写入再读取  | [文件系统](/zh-cn/v0.0.18/docs/stdlib/fs/)；在源文件旁创建文件                        |
| 时间与等待      | [时间](/zh-cn/v0.0.18/docs/stdlib/time/)；明确区分墙上时钟与单调时钟                  |
| HTTP 服务和请求 | [HTTP](/zh-cn/v0.0.18/docs/stdlib/http/)；先启动本地服务，再运行客户端                |
| Task 与 await   | [异步调用](/zh-cn/v0.0.18/docs/guide/concurrency/)；一次性句柄与显式 Result           |
| 多文件项目      | [包与模块](/zh-cn/v0.0.18/docs/guide/packages/)；本地 import                          |
| 数据库查询      | [PG / Redis / MySQL](/zh-cn/v0.0.18/docs/stdlib/database/)；native C 与真实服务       |
| 二进制与 TCP    | [bytes](/zh-cn/v0.0.18/docs/stdlib/bytes/) / [net](/zh-cn/v0.0.18/docs/stdlib/net/)；native C |

仓库 [examples 目录](https://github.com/fengyanweb/ku/tree/v0.0.18-r1/examples)还有更完整的应用示例。
