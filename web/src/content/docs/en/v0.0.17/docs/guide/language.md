---
title: "0.0.17 language and borrowing"
description: "0.0.17 language and borrowing"
kuVersion: "0.0.17"
verifiedAt: "2026-10-21"
sources: ["docs/v0.0.17.md", "docs-site/guide/syntax.md"]
slug: en/v0.0.17/docs/guide/language
---

In 0.0.17, ordinary owned parameters move by default. `&name: T` borrows read-only during a synchronous call. Arguments remain ordinary expressions. Borrowing neither consumes nor implicitly clones the source; the callee does not release it.

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

Save as `borrow.ku` and run `ku run borrow.ku`. Output is `2`, `2`, `Ku`. A borrow cannot assign through its source, move an owned field, escape into storage or be captured. Explicitly clone to return a new owned string; Copy fields such as integers may be read and returned.

Async borrowed parameters, borrowed returns and first-class references are unsupported. The interpreter and synchronous C implement this mode; LLVM does not. See the [0.0.17 syntax reference](https://github.com/fengyanweb/ku/blob/v0.0.17/docs/syntax.md).
