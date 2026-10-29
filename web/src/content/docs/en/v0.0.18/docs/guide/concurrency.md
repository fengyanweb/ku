---
title: 'Tasks and async calls'
description: 'Task startup, await, errors and lifetime.'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources: ['docs/concurrency.md', 'docs/semantics.md']
sidebar:
  order: 1
slug: en/v0.0.18/docs/guide/concurrency
---

A Task is the single-use handle returned by an async function call. Calling starts the task; `await` retrieves its result. Recoverable errors in `T!` still use `?` or `try/catch`.

## Start and await

Save as `tasks.ku` and run `ku run tasks.ku`:

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

Expected output: `40`, then `42`. This program also fits the 0.0.18 native Task subset. Compile with `ku build --backend c tasks.ku -o tasks`; choose `tasks.exe` on Windows. This does not demonstrate asynchronous access to every library.

## Transfer data

Owned arguments transfer ownership into an async call. Clone explicitly if the caller needs its own value. A Task can only move, cannot be cloned, and is consumed by one await. `&name: T` declares a synchronous borrowed parameter; async functions cannot declare borrowed parameters or retain a borrow across await. There are no application `task.spawn`, `Task.new`, `thread.spawn`, or detach APIs.

## Operations that can wait

This table applies to **0.0.18**. The default runner follows the interpreter; native Task uses a separate bounded typed subset.

| Operation                 | Interpreter / default runner                               | Restricted native Task                                                                              |
| ------------------------- | ---------------------------------------------------------- | --------------------------------------------------------------------------------------------------- |
| async calls and await     | Single-use Tasks                                           | Listed combinations of primitives, strings, flat arrays, single-layer Results, and function values  |
| Integer `time.sleep(ms)?` | Blocking-worker wait                                       | Driver timer; no file background worker occupied                                                    |
| Files and directories     | Interpreter operations; not a blanket suspension guarantee | `read/try_read/write/try_write/exists/read_dir` through a fixed background pool                     |
| TCP / TLS client          | No corresponding native client                             | Local `net.client/read/write/close` through the fixed background pool; TLS requires a matching pack |
| HTTP                      | Separate HTTP I/O thread, serial Ku handlers               | No general Task HTTP API; synchronous native servers use a separate path                            |
| Database                  | No database access                                         | No general async database calls; a synchronous native client is not async I/O                       |
| JSON / dynamic values     | Interpreter JSON support                                   | JSON, arbitrary dynamic heap combinations, and Task containers are outside the subset               |

File, TCP, and TLS operations already inside the OS or a foreign library cannot be forcibly killed. Resources return only after the real operation finishes and late results are cleaned up. Native Task connections and bytes must remain local to the current frame; they cannot be Task arguments, return values, or container elements.

## Scheduling

The interpreter and native Task use bounded workers and queues. HTTP uses a single I/O owner and a separate handler execution path. These limits do not provide suspension for arbitrary libraries.

## Cancellation and cleanup

When an owning scope exits, unfinished child Tasks it still owns receive cancellation. A valid move transfers cleanup responsibility. Completed but unawaited results are released too. Cancellation attempts finally cleanup under a shared absolute deadline, normally one second; remaining user statements need not run after that budget expires.

Cancellation and timeout are control termination, not ordinary Results caught as business errors. Cancellation cleanup cannot create Tasks, await, or submit new timers, network waits, or blocking jobs. Cancelling a Task does not prove that an already started OS operation has undone its side effects.

`ku supervise` controls whole-program workers rather than Task handles. See [support and process limits](/en/v0.0.18/docs/support/) and the detailed [concurrency reference](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/concurrency.md).
