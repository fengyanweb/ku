---
title: "0.0.17 语言与借用"
description: "0.0.17 语言与借用"
kuVersion: "0.0.17"
verifiedAt: "2026-10-21"
sources: ["docs/v0.0.17.md", "docs-site/guide/syntax.md"]
slug: zh-cn/v0.0.17/docs/guide/language
---

0.0.17 的普通 owned 参数默认 move；`&name: T` 是同步只读借用，调用仍传普通表达式。借用不消费、不隐式 clone，callee 不释放来源。

```ku
fn Read(&text: str): int {
    return text.byte_len()
}

fn main() {
    text = "Ku"
    println(Read(text))
    println(Read(text))
    println(text)
}
```

保存为 `borrow.ku`，运行 `ku run borrow.ku`，输出 `2`、`2`、`Ku`。不能通过借用赋值、move 出 owned 字段、保存借用或捕获借用；要返回独立 owned 字符串时显式 `.clone()`。整数等 Copy 字段可以读取返回。

该版不支持异步借用参数、借用返回和一等引用。解释器与同步 C 接入该模式；LLVM 未支持。完整写法见[0.0.17 语法规范](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/syntax.md)。
