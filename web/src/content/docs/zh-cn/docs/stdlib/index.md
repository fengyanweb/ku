---
title: '标准库'
description: '按用途查找 Ku 官方标准库与示例。'
kuVersion: '0.0.18'
verifiedAt: '2026-10-21'
sources: ['src/stdlib/metadata.rs', 'src/stdlib/mod.rs', 'docs/syntax.md']
sidebar:
  order: 0
---

Ku 标准库随 CLI 一起分发，直接通过 `std` 导入，无需另装包。按任务选择下列模块；每个参考页列出接口、返回值、错误和示例。

## 数据与集合

| 模块              | 用途                             |
| ----------------- | -------------------------------- |
| [string](string/) | Unicode 字符串、查找、切片与替换 |
| [array](array/)   | 数组读取、组合与映射             |
| [object](object/) | 字符串键对象、字段与默认值       |
| [json](json/)     | JSON 编码、解码与动态值转换      |

## 文件、时间与网络

| 模块              | 用途                             |
| ----------------- | -------------------------------- |
| [fs](fs/)         | UTF-8 文件读写与目录列表         |
| [time](time/)     | 时间戳、日期、格式化、时长与等待 |
| [config](config/) | `.env` 与 YAML 配置              |
| [http](http/)     | HTTP 客户端、路由、请求与响应    |
| [bytes](bytes/)   | 二进制缓冲                       |
| [net](net/)       | TCP 与 TLS 客户端                |

## 数据库与开发工具

| 模块                            | 用途                           |
| ------------------------------- | ------------------------------ |
| [pg / redis / mysql](database/) | 数据库客户端、连接池与结果读取 |
| [lexer / parser](compiler/)     | 词法与语法调试摘要             |

## 导入

```ku
import json from "std.json"
import { http, time } from "std"
```

每页示例都显式导入依赖。数据库、`bytes` 和 `net` 目前使用 native C 后端，其他模块的具体范围见各页及[支持矩阵](/zh-cn/docs/support/)。

`T!` 表示可能返回 Error 的 Result；用 `?` 传播或 `try/catch` 处理。`&` 参数只读借用，不消费原值，也不隐式复制。返回新字符串或数组所需的分配与输入复制是不同的操作。

[示例集](/zh-cn/docs/examples/)按可直接运行的任务组织；[包与模块](/zh-cn/docs/guide/packages/)介绍你自己的库。`std.task` 是内部观测接口，应用程序通过 `async fn` 与 `await` 使用 Task，见[并发指南](/zh-cn/docs/guide/concurrency/)。
