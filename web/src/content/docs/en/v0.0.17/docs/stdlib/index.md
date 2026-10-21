---
title: "0.0.17 standard library"
description: "0.0.17 standard library"
kuVersion: "0.0.17"
verifiedAt: "2026-10-21"
sources: ["docs-site/guide/stdlib.md", "src/stdlib/metadata.rs", "docs/v0.0.17.md"]
slug: en/v0.0.17/docs/stdlib
---

This version includes string, array, object, json, fs, time, config, http, pg, redis, mysql, bytes, net and lexer/parser utilities. Databases, bytes and net require native C. Consult the tagged implementation for individual interfaces.

This example uses this version's `json.stringify` / `json.parse` names.

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

Run `ku run json.ku`; output is `{"count":7,"name":"Ku"}` and `Ku`.

[Full version-specific standard library guide](https://github.com/fengyanweb/ku/blob/v0.0.17/docs-site/guide/stdlib.md) · [Tagged API metadata](https://github.com/fengyanweb/ku/blob/v0.0.17/src/stdlib/metadata.rs)

This release does not include 0.0.18's large-float JSON round-trip correction. Current edge-case behavior is not an older-version guarantee.
