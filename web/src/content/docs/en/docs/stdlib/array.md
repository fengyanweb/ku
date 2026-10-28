---
title: Arrays
description: Typed arrays, lookup, mapping, and non-mutating collection operations.
kuVersion: '0.0.19'
verifiedAt: '2026-10-21'
sources: ['src/stdlib/array.rs', 'src/stdlib/metadata.rs', 'docs/syntax.md']
sidebar:
  order: 2
---

An array holds values of one element type, written `[T]`. Indices start at zero. Use `try_get` when a missing index should be a recoverable error.

## Import

```ku
import array from "std.array"
```

The module name is optional when using methods only. `map` is an instance method rather than `array.map`.

## Reference

| Function or method                     | Returns       | Behavior                                                        |
| -------------------------------------- | ------------- | --------------------------------------------------------------- |
| `array.len(values:[T])`                | `int`         | Element count.                                                  |
| `array.is_empty(values:[T])`           | `bool`        | Whether the count is zero.                                      |
| `array.try_get(values:[T], index:int)` | `T!`          | Owned element value or `array/index_out_of_bounds`.             |
| `array.push(values:[T], value:T)`      | `[T]`         | A new array containing one additional element.                  |
| `array.concat(left:[T], right:[T])`    | `[T]`         | A new array containing both arrays. Interpreter only.           |
| `array.first(values:[T])`              | `T` or `null` | First element, or null for an empty array. Interpreter only.    |
| `array.last(values:[T])`               | `T` or `null` | Last element, or null for an empty array. Interpreter only.     |
| `values.map(mapper)`                   | `[U]`         | Apply a function value to each element and collect its results. |

`push` and `concat` do not modify their input arrays. They may copy elements and allocate a new array; they are not in-place append operations. A direct out-of-bounds `values[index]` is a runtime failure, whereas `try_get` returns a Result. Negative `try_get` indices also return an error.

Methods that copy elements cannot be used on arrays of move-only resource handles, including database and TCP clients. Such arrays can still be moved and released as a whole.

## Example

Save as `arrays.ku`, then run `ku run arrays.ku`. This example also compiles using `ku build --backend c -o arrays arrays.ku`.

```ku
import array from "std.array"

fn main(): null! {
    values = [1, 2, 3]
    extended = values.push(4)
    doubled = extended.map(x => x * 2)
    println(array.len(values))
    println(doubled.try_get(3)?)
    try {
        values.try_get(9)?
    } catch (error) {
        println(error.code)
    }
    return ok(null)
}
```

```text
3
8
index_out_of_bounds
```

`values` still contains three elements after `push`; `extended` contains four. `mapper` must match the element type, and normal ownership and Result rules apply inside it.

## Backend support

The example works in the interpreter and synchronous native C. v0.0.19 native C supports `len`, `is_empty`, `try_get`, `push`, and `map` for supported element and callback types. It does not implement `first`, `last`, or `concat`. General object/function-valued arrays and native Task code have additional backend limits; a successful interpreter run alone does not prove native support.
