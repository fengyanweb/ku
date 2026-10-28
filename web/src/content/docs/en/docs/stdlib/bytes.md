---
title: Binary data
description: Owned bytes, strict UTF-8 conversion, indices, and binary network payloads.
kuVersion: '0.0.19'
verifiedAt: '2026-10-21'
sources:
  ['src/stdlib/metadata.rs', 'src/backend/c.rs', 'tests/native_net_test.rs']
sidebar:
  order: 10
---

`std.bytes` represents owned binary data, including NUL and bytes that are not UTF-8. This module is available in generated native C programs; the interpreter does not implement it.

## Import and reference

```ku
import bytes from "std.bytes"
```

| Function or method               | Returns      | Behavior                                       |
| -------------------------------- | ------------ | ---------------------------------------------- |
| `bytes.from_str(text:str)`       | bytes Result | Copy UTF-8 bytes into an owned binary value.   |
| `bytes.from_array(values:[int])` | bytes Result | Copy integer values; each must be in `0..255`. |
| `data.len()`                     | `int`        | Byte length.                                   |
| `data.get(index:int)`            | `int!`       | Read one byte as `0..255`.                     |
| `data.to_str()`                  | `str!`       | Validate UTF-8 and return an owned string.     |
| `data.clone()`                   | bytes        | Make an independent binary copy.               |

The bytes type is inferred from constructors; there is no separate public type annotation to add. Construction has a 64 MiB maximum. Invalid integer bytes return `bytes/invalid_byte`; negative or out-of-range access returns `bytes/index_out_of_bounds`; invalid text returns `bytes/invalid_utf8`. Fallible allocations can return `bytes/out_of_memory`. These methods do not silently replace malformed text.

## Example

Save as `binary.ku`. Build and run the native binary:

```sh
ku build --backend c -o binary binary.ku
./binary
```

```ku
import bytes from "std.bytes"

fn main(): null! {
    data = bytes.from_array([65, 0, 255])?
    copy = data.clone()
    println(data.len())
    println(copy.get(1)?)
    text = bytes.from_str("中😀")?
    println(text.len())
    println(text.to_str()?)
    try {
        data.to_str()?
    } catch (error) {
        println(error.code)
    }
    return ok(null)
}
```

```text
3
0
7
中😀
invalid_utf8
```

On Windows, select an `.exe` output name and run that file. `ku check binary.ku` checks the source; `ku run binary.ku` is not a bytes implementation. Pass binary values directly to [TCP client `write`](/en/docs/stdlib/net/), which reads them without consuming them.
