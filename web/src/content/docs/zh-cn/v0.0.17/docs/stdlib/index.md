---
title: "0.0.17 标准库"
description: "0.0.17 标准库"
kuVersion: "0.0.17"
verifiedAt: "2026-10-21"
sources: ["docs-site/guide/stdlib.md", "src/stdlib/metadata.rs", "docs/v0.0.17.md"]
slug: zh-cn/v0.0.17/docs/stdlib
---

该版包含 string、array、object、json、fs、time、config、http、pg、redis、mysql、bytes、net 以及 lexer/parser 调试模块。数据库、bytes 和 net 使用 native C；各接口以该标签的实现为准。

下面使用该版 `json.stringify` / `json.parse` 接口，不套用后续版本文档。

```ku
import json from "std.json"

fn main(): null! {
    value = { name: "Ku", count: 7 }
    text = json.stringify(value)?
    println(text)
    decoded = json.parse(text)?
    println(decoded["name"]?.as_str()?)
    return ok(null)
}
```

运行 `ku run json.ku`，输出 `{"count":7,"name":"Ku"}` 和 `Ku`。

[完整该版标准库指南](https://github.com/fengyanweb/ku/blob/v0.0.17/docs-site/guide/stdlib.md) · [该版接口元数据](https://github.com/fengyanweb/ku/blob/v0.0.17/src/stdlib/metadata.rs)

该版不包含 0.0.18 的大浮点 JSON 往返修复；不要将当前版本的边界行为当作旧版保证。
