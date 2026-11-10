---
title: JSON reference
description: Ku JSON encoding, decoding, KuValue conversion, error codes, and backend support.
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'src/stdlib/json.rs',
    'src/stdlib/metadata.rs',
    'docs/syntax.md',
    'examples/json.ku',
    'docs/v0.0.18.md',
  ]
sidebar:
  order: 4
slug: en/v0.0.18/docs/stdlib/json
---

`std.json` encodes and decodes JSON. `json.encode` reads its input without consuming it; `json.decode` parses text into an independently owned dynamic value. Both return Result. `stringify`, `parse`, and `try_parse` remain compatibility names. All of these interfaces are included in 0.0.18.

## Import and interfaces

```ku
import json from "std.json"
```

| Interface                    | Returns    | Meaning                                                  |
| ---------------------------- | ---------- | -------------------------------------------------------- |
| `json.encode(&value:any)`    | `str!`     | Read-only encoding into new compact JSON text            |
| `json.decode(text:str)`      | `KuValue!` | Strict parsing into an independently owned dynamic value |
| `json.stringify(&value:any)` | `str!`     | Compatibility name for `encode`                          |
| `json.parse(text:str)`       | `KuValue!` | Compatibility name for `decode`                          |
| `json.try_parse(text:str)`   | `KuValue!` | Compatibility name for `decode`                          |

`!` denotes Result. Use `?` to propagate errors or `try/catch` to handle them. Encoding does not consume or mutate the input.

## Encode and read values

This is a complete basic example. A more complete example is available in [examples/json.ku](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/examples/json.ku).

```ku
import json from "std.json"

fn main(): null! {
    value = { name: "Ku", count: 7 }
    text = json.encode(value)?
    println(text)

    decoded = json.decode(text)?
    println(decoded["name"]?.as_str()?)
    println(decoded["count"]?.as_int()? + 1)
    return ok(null)
}
```

Object keys are sorted deterministically. Expected output:

```text
{"count":7,"name":"Ku"}
Ku
8
```

A parsed `KuValue` cannot participate directly in integer arithmetic. Convert with `.as_int()?` or `.as_str()?` first. A mismatched type returns an error. Missing object keys also return errors rather than automatic defaults.

## Data and boundaries

Supported inputs include strings, booleans, integers, finite floats, null, nested arrays, objects with string keys, and representable structs. Functions, Tasks, resource handles, Results, enums, and cycles cannot be encoded.

| Rule                  | Current implementation                                                                     |
| --------------------- | ------------------------------------------------------------------------------------------ |
| JSON syntax           | Strict; rejects comments, trailing commas, extra trailing input, and invalid Unicode       |
| Input / output        | Each limited to 1,000,000 bytes                                                            |
| Depth budget          | Root depth is 0; rejects `depth > 32`; 33 empty container levels are accepted, 34 rejected |
| Integers              | Signed 64-bit; overflow is an error, without automatic float conversion                    |
| Floating point        | Finite binary64; original spelling, float tags, and negative zero are not preserved        |
| Unicode               | Chinese, emoji, escapes, embedded NUL                                                      |
| Duplicate object keys | Parsing retains the last value                                                             |
| Key order             | Encoding sorts by UTF-8 byte order                                                         |

A float such as `1.0` may encode as `1`. When an integer-looking float is outside int64, the encoder adds `.0` to keep decoding on the float path: `1e20` encodes as `100000000000000000000.0` and decodes again. Finite binary64 values do not preserve original spelling, a float tag or the sign of negative zero.

## Errors

Recoverable errors use the `json` domain. Common codes are `parse_error`, `stringify_error`, and `out_of_memory`. A parse message's `byte N` is a zero based UTF-8 byte offset. See [errors and troubleshooting](/en/v0.0.18/docs/guide/errors/) for a minimal invalid input handler.

## Backend support

The interpreter, default packaged runner, and synchronous native C backend have JSON implementations. **The restricted native Task subset and LLVM do not support JSON.** Default runner imports still depend on their source locations; JSON support does not imply a fully independent package.

See the complete [JSON syntax reference](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/syntax.md#127-json).
