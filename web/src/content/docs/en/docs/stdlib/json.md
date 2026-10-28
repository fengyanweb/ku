---
title: JSON reference
description: Ku JSON encoding, decoding, KuValue conversion, error codes, and backend support.
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-28'
sources:
  [
    'src/stdlib/json.rs',
    'src/stdlib/metadata.rs',
    'docs/syntax.md',
    'examples/json.ku',
    'docs/v0.0.19.md',
    'src/ir/task_lower.rs',
    'src/backend/c_task.rs',
    'src/runtime/interpreter.rs',
  ]
sidebar:
  order: 4
---

`json.encode` / `json.decode` share the `stringify` / `parse` implementations. Compatibility names remain available. The previous JSON implementation remains the default; yyjson is not used.

## Import and interfaces

```ku
import json from "std.json"
```

| Interface                    | Returns    | Meaning                                                  |
| ---------------------------- | ---------- | -------------------------------------------------------- |
| `json.stringify(&value:any)` | `str!`     | Read only encoding into new compact JSON text            |
| `json.parse(text:str)`       | `KuValue!` | Strict parsing into an independently owned dynamic value |
| `json.try_parse(text:str)`   | `KuValue!` | Compatibility alias for `parse`                          |
| `json.encode(&value:any)`    | `str!`     | Alias for `stringify`                                    |
| `json.decode(text:str)`      | `KuValue!` | Alias for `parse`                                        |

`!` denotes Result. Use `?` to propagate errors or `try/catch` to handle them. The general synchronous encoding interface does not consume or mutate its input. The native Task literal subset below has separate ownership restrictions.

## Encode and read values

This is a complete basic example. A more complete example is available in [examples/json.ku](https://github.com/fengyanweb/ku/blob/main/examples/json.ku).

```ku
import json from "std.json"

fn main(): null! {
    value = { name: "Ku", count: 7 }
    text = json.stringify(value)?
    println(text)

    decoded = json.parse(text)?
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

## Reading dynamic values

| Operation                      | Returns               | Failure                                                               |
| ------------------------------ | --------------------- | --------------------------------------------------------------------- |
| `value["key"]` followed by `?` | Owned `KuValue` field | Missing key or invalid receiver                                       |
| `value.as_int()`               | `int!`                | `value/type_mismatch` unless already an integer; no string conversion |
| `value.as_str()`               | `str!`                | `value/type_mismatch` unless already a string                         |
| `value.take("key")`            | `KuValue!`            | Consumes the object to take a subtree; missing key/type errors        |

There is no implicit number/string coercion or default value on missing keys. These methods are distinct from typed object field access and `get_or`.

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

Recoverable errors use the `json` domain. Common codes are `parse_error`, `stringify_error`, and `out_of_memory`. A parse message's `byte N` is a zero based UTF-8 byte offset. See [errors and troubleshooting](/en/docs/guide/errors/) for a minimal invalid input handler.

## Backend support

The interpreter, default packaged runner and synchronous native C backend support the general interfaces above. The native Task path supports only the encoding subset below; generic Task parsing/decoding is not supported. LLVM does not provide this library. Default runner imports still depend on source locations.

See the complete [JSON syntax contract](https://github.com/fengyanweb/ku/blob/main/docs/syntax.md#127-json).

## Scalar JSON objects in native Tasks

Save as `task-json.ku` and compile with `ku build --backend c --profile release task-json.ku -o task-json`:

```ku
import json from "std.json"

async fn main(): null! {
    text = json.encode({message: "中😀", count: 2, ready: true, missing: null})?
    println(text)
    return ok(null)
}
```

Expected output is `{"count":2,"message":"中😀","missing":null,"ready":true}`.

Native Task `json.encode` / `json.stringify` accepts exactly one **object literal**, with at most 64 distinct keys and only `int`, `bool`, `null` or `str` values. Fields are evaluated left to right, then keys are emitted in deterministic order. Owned string expressions move as they normally do when constructing this literal; explicitly clone a string if it must remain available.

An existing dynamic object variable, arrays, nested objects and floats are outside this lowering path. Duplicate literal keys are rejected during compilation; this differs from general JSON parsing, which retains the last duplicate. Result errors still require `?` or `try/catch`. This subset does not replace the default JSON implementation or enable arbitrary JSON operations in async code.
