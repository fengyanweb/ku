---
title: JSON reference
description: Ku JSON encoding, decoding, KuValue conversion, error codes, and backend support.
kuVersion: '0.0.20'
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

`std.json` encodes and decodes JSON. The general synchronous `json.encode` interface reads its input without consuming it; `json.decode` parses text into an independently owned dynamic value. Both return Result. `stringify`, `parse`, and `try_parse` remain compatibility names. These interfaces remain available in 0.0.20. The previous JSON implementation remains the default; yyjson is not used.

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

`!` denotes Result. Use `?` to propagate errors or `try/catch` to handle them. Encoding does not consume or mutate its input. The Task section below explains local JSON ownership and type restrictions.

## Encode and read values

This is a complete basic example. A more complete example is available in [examples/json.ku](https://github.com/fengyanweb/ku/blob/v0.0.20/examples/json.ku).

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

The interpreter, Runner and synchronous native C backend support the general interfaces above. Native Tasks support local JSON decoding, string-key reads, subtree transfer, conversions and re-encoding within the scope below. LLVM does not provide this library. Runner imports still depend on source locations.

See the complete [JSON syntax contract](https://github.com/fengyanweb/ku/blob/v0.0.20/docs/syntax.md#127-json).

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

This scalar-field example remains a minimal starting point. Dynamic object literals constructed inside Tasks allow at most 64 distinct keys and evaluate fields left to right. Duplicate literal keys are rejected during compilation, unlike JSON decoding, which retains the last duplicate. Owned strings and dynamic JSON fields move into the constructed object under the existing ownership rules.

## Local values across waits in native Tasks

0.0.20 already supports local `json.decode()` results surviving `await`, string-key indexing, `take`, `as_int`, `as_str` and `json.encode`. The following is the released [examples/task_json.ku](https://github.com/fengyanweb/ku/blob/v0.0.20/examples/task_json.ku):

```ku
import json from "std.json"
import time from "std.time"
async fn tick():int! { time.sleep(2)? return ok(1) }
async fn main():null! {
    decoded=json.decode("{\"counter\":7,\"data\":{\"message\":\"中文😀\",\"items\":[1,true,null]}}")?
    selected=decoded["counter"]?
    counter=selected.as_int()?
    copied=decoded["data"]?
    next=(await tick())?
    data=decoded.take("data")?
    body=json.encode({counter:counter+next,id:"rid",data:data,copy:copied})?
    println(body)
    return ok(null)
}
```

After the wait, the encoded output's `counter` field is 8. Both `data` and `copy` retain the nested object, Chinese text, emoji and decoded array. Results still use `?` or `try/catch`, without implicit unwrapping. See the complete [Task JSON guide](https://github.com/fengyanweb/ku/blob/v0.0.20/docs/native-task-json.en.md).

- `json.encode(value)` reads its input; the original remains usable afterward.
- `value["key"]` returns an independently owned value, which remains valid after releasing its original root.
- `value.take("key")` consumes the original root and transfers the subtree. Do not read the consumed root afterward.
- `as_int()` and `as_str()` consume their conversion input. Dynamic fields move into an output object when it is constructed.
- Allocations prepay the Task byte budget and refund only after physical release. Returns, errors, cancellation and deadlines clean up their owners. This ledger covers Ku-requested heap bytes, not RSS, allocator internals or all third-party memory.

The existing default JSON engine, strict syntax and depth rules above are unchanged. Input and output are each limited to 1,000,000 bytes. Insufficient budget returns an error. An individual JSON operation is size-bounded but does not suspend internally; this is not instruction-level preemption.

Decoded JSON objects, arrays, strings, numbers, booleans and null can be retained, transferred and re-encoded. This does not add integer indexing of dynamic JSON or heterogeneous array literals. Task parameters, public outcomes and closure captures have not been expanded to arbitrary JSON; native Task floating-point locals and unlisted operations retain their existing subset restrictions. Unsupported programs are rejected during compilation.
