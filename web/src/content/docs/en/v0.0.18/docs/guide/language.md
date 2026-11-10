---
title: Language guide
description: Learn Ku variables, functions, control flow, ownership, modules, and recoverable errors.
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/syntax.md',
    'docs/semantics.md',
    'examples/function.ku',
    'examples/control_flow.ku',
    'examples/result.ku',
  ]
sidebar:
  order: 0
slug: en/v0.0.18/docs/guide/language
---

This guide introduces core syntax. The repository's [syntax reference](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/syntax.md) and [language semantics](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/semantics.md) remain authoritative. These are teaching fragments; see [backend support](/en/v0.0.18/docs/support/) for differences.

## Variables and types

```ku
name = "Ku"
count:int = 3
ready:bool = true
values:[int] = [1, 2, 3]
count += 1
```

The first assignment declares a variable. Primitive types are `int`, `float`, `bool`, `str`, and `null`. Integers are signed 64-bit; floating point uses binary64. Integers do not become conditions through truthy conversion.

## Functions and control flow

This complete basic example comes from `examples/function.ku`:

```ku
fn add(a: int, b: int): int {
    return a + b
}

fn main() {
    println(add(10, 20))
}
```

Expected output: `30`. This control flow fragment can be placed inside `main`:

```ku
total = 0
for i in 5 {
    total += i
}
if (total == 10) {
    println("sum ok")
}
```

`for i in 5` iterates from `0` through `4`. `if` and `while` require `bool` conditions. Use braces for multiple statements. Branch expressions use `match`; there is no `switch`.

<span id="ownership"></span>

## Ownership and read only borrowing

`int`, `float`, `bool`, and `null` are Copy values. Strings, arrays, objects, structs, enums, Results, and function values are Owned. Assignment and ordinary arguments move owned values by default. Use `.clone()` when you need an independent value. Tasks and move-only native handles cannot be cloned. Owned values are dropped deterministically when their owning scope exits; copying is not implicit.

```ku
original = "Ku"
copy = original.clone()
println(original)
println(copy)
```

Ku ownership does not depend on a global tracing garbage collector. Some shared internal structures still use reference counting, including native closure environments.

A synchronous function can declare a read only borrowed parameter. This complete example keeps the caller’s value:

```ku
fn length(&text: str): int {
    return text.len()
}

fn main() {
    text = "Ku"
    println(length(text))
    println(text)
}
```

Expected output: `2`, then `Ku`.

Call it as `length(value)`, without `&` on the argument. Borrowed data cannot be mutated, moved out, or escape the call. Combinations such as borrowed async parameters remain unsupported. An ordinary Owned parameter can still move its argument even when the function body only reads it. The parameter declaration determines whether ownership is retained. Read only borrowing does not prohibit I/O or internal state changes.

## Structs, objects, and modules

```ku
struct User {
    name: str
    age: int
}

user = User { name: "Ku", age: 1 }
println(user.name)
```

Dynamic objects use `{ name: "Ku" }`. A normal missing object index fails. `value["name"]?` handles lookup through Result, yielding a `KuValue` on success. Use `.as_str()?` or `.as_int()?` for a concrete type. A missing key does not silently become `null`.

Standard library imports use dotted paths, such as `import json from "std.json"`. Local modules can use `import math from "./math"`. User defined top level exports start with an ASCII uppercase letter; lowercase names remain private to the file.

<span id="errors"></span>

## Recoverable errors

The return type `T!` can return either T or Error. `?` propagates a recoverable error; `fail` creates one:

```ku
fn read_name(): str! {
    fail "name missing"
}

fn main() {
    try {
        println(read_name()?)
    } catch (error) {
        println(error.message)
    }
}
```

Error has `domain`, `code`, and `message` fields. Ordinary `catch` does not catch `panic`, Task cancellation, or timeout. Cancellation cleanup in finally shares an absolute deadline budget; it does not guarantee completion of arbitrary user cleanup.

## Concurrency boundaries

Application code obtains a single use Task handle by calling an `async fn`, then uses `await`. There are no application APIs named `task.spawn`, `Task.new`, or `thread.spawn`. The interpreter and native Task subset have different coverage. An example working in one backend does not prove full support in another. See [Task and async calls](/en/v0.0.18/docs/guide/concurrency/) and [platform and backend support](/en/v0.0.18/docs/support/).
