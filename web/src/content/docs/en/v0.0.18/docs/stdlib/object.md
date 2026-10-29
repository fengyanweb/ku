---
title: Objects
description: String-key objects, strict lookup, explicit defaults, and JSON dynamic values.
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources: ['src/stdlib/object.rs', 'src/stdlib/metadata.rs', 'docs/syntax.md']
sidebar:
  order: 3
slug: en/v0.0.18/docs/stdlib/object
---

Object literals associate string keys with values. Use `get_or` for an explicit fallback, or a Result lookup when a missing field is an error.

## Import and reference

```ku
import object from "std.object"
```

| Form                                      | Result                      | Missing key                                              |
| ----------------------------------------- | --------------------------- | -------------------------------------------------------- |
| `object.get_or(value, key:str, fallback)` | Stored value or fallback    | Returns fallback.                                        |
| `value.get_or(key:str, fallback)`         | Same operation              | Returns fallback.                                        |
| `value["key"]?`                           | Unwrapped successful lookup | Returns `object/missing_key` through `?`.                |
| `value["key"]`                            | Stored value                | Runtime failure.                                         |
| `value.key`                               | Field access                | Requires a suitable known field; not an optional lookup. |

`get_or` does not insert a missing key or mutate the object. A present key whose value is `null` returns null rather than the fallback. The fallback expression is evaluated normally before the function call: this is not lazy `get_or_else`. The fallback is an owning argument, so use `.clone()` only if an owned fallback must also remain independently available afterward.

## Example

Save as `objects.ku`, then run `ku run objects.ku` or `ku build --backend c -o objects objects.ku`.

```ku
fn main(): null! {
    user = { name: "Ku" }
    println(user.get_or("name", "unknown"))
    println(user.get_or("age", 99))
    try {
        value = user["missing"]?
        println(value)
    } catch (error) {
        println(error.code)
    }
    return ok(null)
}
```

```text
Ku
99
missing_key
```

## JSON objects

`json.decode(text)?` returns a `KuValue`, not a statically typed object. Use `decoded["name"]?.as_str()?` or `.as_int()?` before typed operations. See the [JSON reference](/en/v0.0.18/docs/stdlib/json/) for a complete runnable example and malformed-input handling.

## Backend support

The example uses receiver-method lookup supported by the interpreter and synchronous native C. For native C, use `value.get_or(...)`; the module-level `object.get_or(...)` form does not currently lower correctly. A method-only program does not need the named import. Native C does not support every expression whose inferred type remains unknown, including some ordinary dot-field accesses on object literals. Use typed structs for known layouts; a method or field example accepted by the interpreter is not automatically portable. LLVM and native Task have additional limits.
