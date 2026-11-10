---
title: '词法与语法调试'
description: 'lexer.scan 与 parser.parse 调试接口。'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources: ['src/stdlib/mod.rs', 'src/stdlib/metadata.rs']
sidebar:
  order: 12
slug: zh-cn/v0.0.18/docs/stdlib/compiler
---

`std.lexer` 和 `std.parser` 用于查看源码处理的调试摘要，不是稳定的公开 Token 或 AST 数据模型。

| 接口                          | 返回    | 含义                                        |
| ----------------------------- | ------- | ------------------------------------------- |
| `lexer.scan(text: str)`       | `[str]` | token 种类的调试文本数组                    |
| `parser.parse(source: str)`   | `str`   | 解析源程序并返回摘要                        |
| `parser.parse(tokens: [str])` | `str`   | 返回 token 数组的数量摘要，不重新解析 token |

```ku
import { lexer, parser } from "std"

fn main() {
    source = "fn main() { println(1) }"
    println(lexer.scan(source))
    println(parser.parse(source))
}
```

该示例使用解释器 `ku run`。语法错误作为运行错误报告，接口不返回 `T!`，不能把失败转换成普通 Result。native C 和 LLVM 不支持这两个调试模块。

日常编译诊断优先使用 `ku check`，查看 lowered IR 使用 `ku ir`；见[命令行工具](/zh-cn/v0.0.18/docs/tools/cli/)。
