---
title: Language guide
description: Learn Ku variables, functions, control flow, ownership, modules, and recoverable errors.
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-28'
sources:
  [
    'docs/syntax.md',
    'docs/semantics.md',
    'examples/function.ku',
    'examples/control_flow.ku',
    'examples/result.ku',
    'src/cli.rs',
    'src/ir/task_lower.rs',
  ]
sidebar:
  order: 0
---

This guide introduces core syntax. The repository's [syntax contract](https://github.com/fengyanweb/ku/blob/main/docs/syntax.md) and [semantic contract](https://github.com/fengyanweb/ku/blob/main/docs/semantics.md) remain authoritative. These are teaching fragments; see [backend support](/en/docs/support/) for differences.

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

## Ownership and read only borrowing

`int`, `float`, `bool`, and `null` are Copy values. Strings, arrays, objects, structs, enums, Results, and function values are Owned. Assignment and ordinary arguments move owned values by default. Use `.clone()` when you need an independent value. Tasks cannot be cloned.

```ku
original = "Ku"
copy = original.clone()
println(original)
println(copy)
```

A synchronous function can declare a read only borrowed parameter:

```ku
fn length(&text: str): int {
    return text.len()
}
```

Call it as `length(value)`, without `&` on the argument. Borrowed data cannot be mutated, moved out, or escape the call. Combinations such as borrowed async parameters remain unsupported. Read only borrowing does not guarantee arbitrary function purity.

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

## Recoverable errors

The return type `T!` is a Result: success contains a `T`, and failure contains an Error. After a call, `?` extracts the successful value; on failure it transfers control to an enclosing `try/catch`, or returns the error from the current `T!` function. Do not append `?` to an ordinary value. `fail` creates a recoverable error:

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

Here `main` uses a plain declaration because the error is caught inside it. Declare `T!` when errors should reach the caller; for an entry point this is `fn main(): null!`. The type suffix `!` is different from boolean negation such as `!ready`. See [error handling](/en/docs/guide/errors/#result-operators).

Error has `domain`, `code`, and `message` fields. Ordinary `catch` does not catch `panic`, Task cancellation, or timeout. Cancellation cleanup in finally shares an absolute deadline budget; it does not guarantee completion of arbitrary user cleanup.

## Concurrency boundaries

Application code obtains a single use Task handle by calling an `async fn`, then uses `await`. There are no application APIs named `task.spawn`, `Task.new`, or `thread.spawn`. The interpreter and native Task subset have different coverage. An example working in one backend does not prove full support in another. See [platform and backend support](/en/docs/support/).

## Select the execution contract

The source language and backend coverage are related but separate. `ku check` checks a program; `ku run` executes the interpreter; default `ku build` packages that interpreter path; `ku build --backend c` lowers only its supported subset. A library call accepted by the checker can still be rejected by native lowering.

Read `T!` as a Result value and `async fn` as a Task-producing function. `?` propagates a Result; `await` consumes a Task. They are not interchangeable. Operations such as native HTTP PG queries have explicitly supported suspension paths and must be used exactly as their module describes.

Start from the [standard library index](/en/docs/stdlib/) for parameter/return/error tables, then check [backend and resource boundaries](/en/docs/support/) before selecting deployment commands. No general-purpose thread-spawn or automatic retry API is implied by async syntax.
