---
title: 'Tasks and async calls'
description: 'Task startup, await, errors and lifetime.'
kuVersion: '0.0.19'
verifiedAt: '2026-10-28'
sources:
  [
    'docs/concurrency.md',
    'docs/semantics.md',
    'src/runtime/interpreter.rs',
    'src/runtime/task.rs',
    'src/ir/task_lower.rs',
  ]
sidebar:
  order: 1
---

Calling an async function submits work and returns a one-use Task handle. Work may run before `await`; submission is not a guarantee of immediate execution or ordering. `await` obtains its result; `T!` errors still require `?` or `try/catch`.

```ku
async fn Double(value: int): int! {
    return ok(value * 2)
}

async fn main(): null! {
    first = Double(20)
    second = Double(21)
    println((await first)?)
    println((await second)?)
    return ok(null)
}
```

Save as `tasks.ku` and run `ku run tasks.ku`. It prints `40` and `42`; the example is also within the restricted native Task subset.

## Ownership and completion

A Task moves; it cannot be cloned or awaited twice. When its owning scope exits, unfinished child tasks receive cancellation. Cancellation attempts `finally` cleanup within a shared one-second deadline. Cancellation and timeout are not ordinary Results caught as business errors.

`&` borrowing is synchronous; async functions cannot declare borrowed parameters. An async task must own its input data.

## Result is not Task

| Expression       | Meaning                                                                        |
| ---------------- | ------------------------------------------------------------------------------ |
| `Double(20)`     | A Task handle for the async call                                               |
| `await first`    | Consume that handle and obtain the declared result, here `int!`                |
| `(await first)?` | Propagate a recoverable error or obtain the integer                            |
| `time.sleep(1)?` | One supported wait returning `null!`, not a handle that needs a second `await` |

A synchronous function returning `T!` is not made asynchronous by `?`. In the native HTTP PG subset, `client.query(...)?` is a compiler-supported suspension, also distinct from a user Task handle.

## Waiting and backends

[time.sleep](/en/docs/stdlib/time/) and supported file operations use their respective execution paths. Complete M:N, general network suspension and arbitrary dynamic heap combinations are not implemented. This arithmetic example does not imply async support for every library, database or `net` call. Supported named [async HTTP handlers](/en/docs/stdlib/http/#named-async-native-handlers), [PG query suspension](/en/docs/stdlib/database/#async-pg) and [scalar JSON encoding](/en/docs/stdlib/json/#scalar-json-objects-in-native-tasks) each have narrower contracts.

`ku supervise` manages whole-program workers, a different unit from a Task handle. See [support and process limits](/en/docs/support/).
