---
title: 'Tasks and async calls'
description: 'Task startup, await, errors and lifetime.'
kuVersion: '0.0.18'
verifiedAt: '2026-10-21'
sources: ['docs/concurrency.md', 'docs/semantics.md']
sidebar:
  order: 1
---

Calling an async function starts a Task and returns a one-use handle. `await` obtains its result; `T!` errors still require `?` or `try/catch`.

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

## Waiting and backends

[time.sleep](/en/docs/stdlib/time/) and supported file operations use their respective execution paths. Complete M:N, general network suspension and arbitrary dynamic heap combinations are not implemented. This arithmetic example does not imply async support for every library, database or `net` call.

`ku supervise` manages whole-program workers, a different unit from a Task handle. See [support and process limits](/en/docs/support/).
