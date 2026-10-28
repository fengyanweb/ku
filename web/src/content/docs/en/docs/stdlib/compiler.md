---
title: 'Lexer and parser utilities'
description: 'Debugging with lexer.scan and parser.parse.'
kuVersion: '0.0.19'
verifiedAt: '2026-10-21'
sources: ['src/stdlib/mod.rs', 'src/stdlib/metadata.rs']
sidebar:
  order: 12
---

`std.lexer` and `std.parser` expose debugging summaries. They are not stable public Token or AST data models.

| Interface                     | Returns | Purpose                                                |
| ----------------------------- | ------- | ------------------------------------------------------ |
| `lexer.scan(text: str)`       | `[str]` | Debug token-kind descriptions                          |
| `parser.parse(source: str)`   | `str`   | Parse source and summarize it                          |
| `parser.parse(tokens: [str])` | `str`   | Return a token-count summary without parsing the array |

```ku
import { lexer, parser } from "std"

fn main() {
    source = "fn main() { println(1) }"
    println(lexer.scan(source))
    println(parser.parse(source))
}
```

Run this example with the interpreter. Invalid syntax is reported as a runtime error; these interfaces do not return `T!`. Native C and LLVM do not support these debugging modules.

For normal diagnostics use `ku check`; inspect lowered IR with `ku ir`. See [command-line tools](/en/docs/tools/cli/).
