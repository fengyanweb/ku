---
title: Strings
description: Unicode text, searching, slicing, transformations, and backend support.
kuVersion: '0.0.18'
verifiedAt: '2026-10-21'
sources: ['src/stdlib/string.rs', 'src/stdlib/metadata.rs', 'docs/syntax.md']
sidebar:
  order: 1
---

Use string functions to inspect UTF-8 text or produce a new string. Read-only inspections leave the original available. Module calls and methods, such as `string.len(text)` and `text.len()`, refer to the same operation.

## Import

```ku
import string from "std.string"
```

Only import a named module when using the module name; method-only code does not need this import. Ku reports unused named imports.

## Reference

The following are documentation signatures; `!` means a Result that can be propagated with `?`.

| Function                                     | Returns | Behavior                                            |
| -------------------------------------------- | ------- | --------------------------------------------------- |
| `string.len(text:str)`                       | `int`   | Unicode scalar count.                               |
| `string.byte_len(text:str)`                  | `int`   | UTF-8 byte count.                                   |
| `string.chars(text:str)`                     | `[str]` | One independently usable string per Unicode scalar. |
| `string.contains(text:str, needle:str)`      | `bool`  | Exact substring search.                             |
| `string.starts_with(text:str, prefix:str)`   | `bool`  | Exact prefix check.                                 |
| `string.ends_with(text:str, suffix:str)`     | `bool`  | Exact suffix check.                                 |
| `string.slice(text:str, start:int, end:int)` | `str!`  | Half-open scalar range `[start, end)`.              |
| `string.replace(text:str, from:str, to:str)` | `str`   | Replace all matches.                                |
| `string.trim(text:str)`                      | `str`   | Remove leading and trailing whitespace.             |
| `string.lower(text:str)`                     | `str`   | Produce lowercase text.                             |
| `string.upper(text:str)`                     | `str`   | Produce uppercase text.                             |

Length counts Unicode scalars, not user-perceived grapheme clusters. A combined emoji or a letter followed by a combining mark may count as more than one scalar. Slice indices are not byte offsets. Negative indices, `start > end`, or an end past the scalar count return `string/slice_error`.

## Example

Save as `strings.ku`, then run `ku run strings.ku` or compile with `ku build --backend c -o strings strings.ku`.

```ku
import string from "std.string"

fn main(): null! {
    text = "中😀 Ku"
    println(string.len(text))
    println(text.byte_len())
    println(text.contains("Ku"))
    println(text.slice(0, 2)?)
    letters = text.chars()
    println(letters[1])
    println(text)
    try {
        text.slice(9, 10)?
    } catch (error) {
        println(error.code)
    }
    return ok(null)
}
```

```text
5
10
true
中😀
😀
中😀 Ku
slice_error
```

## Backend support

The example works in the interpreter and synchronous native C. Unicode-aware `trim`, `lower`, and `upper` are interpreter operations in v0.0.18; native C rejects them. Other entries above have native C implementations. Do not infer general native Task or LLVM support from this synchronous example.
