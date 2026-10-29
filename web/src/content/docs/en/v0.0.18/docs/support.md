---
title: Platform and backend support
description: Distinguish Ku release platforms, interpreter, runner, native C, Task subset, and LLVM support.
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/syntax.md',
    'docs/v0.0.18.md',
    'docs/v0.0.18-worklog.md',
    'docs/protocol-foundation.md',
    '.github/workflows/native-three-os.yml',
  ]
slug: en/v0.0.18/docs/support
---

Ku 0.0.18 is experimental and is not recommended for production use.

## Release platforms

Prebuilt archives are provided for Windows x64 (MSVC), Linux x64 (GNU), and macOS arm64 (Apple Silicon). Each platform uses its own archive; see [installation](/en/v0.0.18/docs/install/).

<span id="execution"></span>

## Execution backends

| Capability                 | Interpreter `ku run` / default runner                              | Synchronous native C                                          | Restricted native Task                             | LLVM text prototype      |
| -------------------------- | ------------------------------------------------------------------ | ------------------------------------------------------------- | -------------------------------------------------- | ------------------------ |
| Functions and control flow | Implemented language paths                                         | Explicitly listed subset                                      | Bounded typed subset                               | Limited prototype        |
| JSON                       | Supported                                                          | Supported                                                     | Unsupported                                        | Unsupported              |
| Local imports              | Dependencies read from source at runtime; runner embeds only entry | Complete local graph expanded at generation                   | Expanded within supported subset                   | Not a deployment backend |
| async / await              | Single-use Tasks and resource limits                               | Dedicated Task lowering or rejection                          | Partial, not full async                            | No full support          |
| HTTP                       | Client/server and `bind/run/close`                                 | Server `listen` subset; no `http.json` or module-level client | No general Task HTTP API                           | Unsupported              |
| Database                   | No database access                                                 | Native-only clients                                           | No general async database calls                    | Unsupported              |
| TCP / TLS client           | No corresponding native client                                     | Supported; TLS needs a matching pack                          | Local handles wait through a fixed background pool | Unsupported              |

`run/check` need no Rust/C compiler. Default build requires matching Rust 1.89.0, a linker, and the complete bundled dependencies. Native C requires the target C toolchain. Successful frontend checking does not prove every backend can compile a program. See [CLI](/en/v0.0.18/docs/tools/cli/), [HTTP](/en/v0.0.18/docs/stdlib/http/), and [Tasks](/en/v0.0.18/docs/guide/concurrency/).

## Important limits

- Native Tasks support integer timers, listed file/directory operations, and local TCP/bytes operations. Background waiting does not establish general network readiness, arbitrary library suspension, or all language combinations.
- HTTP uses one I/O owner and a separate handler execution path.
- Database clients use `module.client(config)?` with bounded pools. Redis is plaintext TCP; MySQL lacks consistent certificate and hostname verification across Oracle/MariaDB. Use loopback, controlled private networks, or a verified TLS tunnel. Remote PG needs `sslmode=verify-full` and trusted roots.
- Self hosted lexer/parser support remains a bounded subset, with Rust supplying the bootstrap toolchain. Version 0.0.x does not promise source or generated C/FFI ABI compatibility. Rebuild native artifacts after updating.

## Known issues

### HTTP 504

Intermittent HTTP 504 responses have been observed. Short runs did not reproduce the issue; its root cause remains unconfirmed and formal 30-minute stability acceptance has not passed for this release.

### Other boundaries

A complete real-database platform matrix, generic Task network scheduling, and arbitrary dynamic heap combinations are outside the supported subset. Task count, connection count, and transport buffer limits have separate scopes; none is a universal whole-process RSS limit.

## Process isolation and memory limits

| Host    | `ku supervise` guarantee and prerequisite                                                                                                                                                                                                                                                                   |
| ------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Linux   | cgroup v2 kernel charged memory; a writable, delegated domain root with `memory` and `pids` enabled is required. Required interfaces include `memory.max`, `memory.swap.max`, `memory.oom.group`, `pids.max` and `cgroup.kill`. Missing controls or permissions cause refusal before application execution. |
| Windows | Nested Job Object limits for individual workers and the worker group, measured as committed virtual memory, not RSS. Initial process commit is checked before its suspended application thread is released. Host policy can reject Job assignment.                                                          |
| macOS   | This isolation mode is not supported natively and is explicitly rejected before starting the application. Use a suitably configured Linux host/VM when required; there is no automatic VM bridge.                                                                                                           |

Worker accounting covers applicable Ku and third-party library allocations in that worker's resource domain. Linux cgroup accounting and Windows commit are different metrics. Linux `memory.max` may permit transient overshoot; these mechanisms do not promise an absolute ceiling on all physical memory at every instant.

The coordinator itself has no OS memory hard limit. Its bounded admission/control reserve is an accounting policy, not a coordinator RSS limit. A single Task's entire heap is not precisely metered; shared connection pools, caches and library allocations cannot be exactly attributed to each Task. External databases, the host/VM and build compilers lie outside the worker domain.

When an OS limit terminates a worker, the coordinator reports failure and only returns reserved capacity after confirmed cleanup. Some allocation failures can be reported before allocation, but third-party exhaustion is not guaranteed to become an ordinary Result. **Database operations are never automatically retried**; an exited worker does not prove rollback, and side effects may be unknown.

See the [process execution reference](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/process-execution.md), [concurrency reference](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/concurrency.md).

## Run a controlled program

After preparing dependencies and an ordinary `hello.ku`, Linux requires a real delegated root supplied by the administrator or service manager:

```sh
ku supervise --worker-memory-bytes 134217728 --total-memory-bytes 536870912 \
  --max-workers 2 --max-queued-jobs 32 --max-queued-bytes 262144 \
  --max-output-bytes 1048576 --deadline-ms 30000 \
  --cgroup-root "$KU_CGROUP_ROOT" -- ./hello.ku
```

On Windows, use the same numeric limits and omit `--cgroup-root`. Decimal byte and millisecond values are required. `--` separates complete-program inputs; this is not a request-routing or general network scheduler API. The command emits configuration, per-execution and summary JSONL records and returns nonzero if a program fails. Native programs are built first and submitted with `--native`; compiler memory is outside the worker limit.
