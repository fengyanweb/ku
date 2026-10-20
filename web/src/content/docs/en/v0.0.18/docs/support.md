---
title: Platform and backend support
description: Distinguish Ku release platforms, interpreter, runner, native C,
  Task subset, and LLVM support.
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  - docs/syntax.md
  - docs/v0.0.18.md
  - docs/v0.0.18-worklog.md
  - docs/protocol-foundation.md
  - .github/workflows/native-three-os.yml
slug: en/v0.0.18/docs/support
---

This documentation describes the frozen scope of the v0.0.18 experimental prerelease. Actual publication and downloadable assets are recorded in GitHub Releases.

## Published CLI platforms

The v0.0.18 experimental prerelease covers Windows x64 (MSVC), Linux x64 (GNU), and macOS arm64 (Apple Silicon). Each platform has its own archive. There is no single executable for all three systems. No other platform assets were verified; see [installation](/en/v0.0.18/docs/install/).

## Execution paths

| Capability                                     | Interpreter `ku run`             | Default `ku build` runner           | Synchronous native C                                | Development native Task                | LLVM text prototype   |
| ---------------------------------------------- | -------------------------------- | ----------------------------------- | --------------------------------------------------- | -------------------------------------- | --------------------- |
| Basic language, functions, control flow        | Implemented                      | Interpreter path                    | Implemented subset                                  | Restricted typed subset                | Limited prototype     |
| JSON                                           | Supported                        | Supported                           | Supported                                           | Unsupported                            | Unsupported           |
| Source independent complete local import graph | Loads source                     | Unsupported; only entry is embedded | Expanded during generation                          | Expanded within supported subset       | No deployment promise |
| async / await                                  | Implemented with resource limits | Interpreter path                    | Dedicated subset or rejection                       | Partial, not full async                | No full support       |
| PostgreSQL / Redis / MySQL clients             | No database access               | No interpreted database access      | Native only                                         | Not general Task waiting               | Unsupported           |
| TCP / TLS client                               | No corresponding native path     | No corresponding native path        | Generated C implementation; TLS needs matching pack | No complete generic network wait claim | Unsupported           |

Default builds require the matching Rust toolchain and linker. Native C requires a target C toolchain. Passing `ku check` does not prove either build works. An LLVM command does not imply a complete release backend.

## The native Task subset

The current development branch has a restricted `async fn main(): null!` source path. It covers explicitly listed primitives, strings, flat arrays, single layer Results, Task start/move/await, some control flow, and bounded file/directory background operations. It is not a complete async backend in the published v0.0.18 package.

Generic Task network waiting, complete M:N execution, arbitrary dynamic heap combinations, all async capture/function combinations, and complete long duration soak remain unfinished. See the precise accepted and rejected subset in the [concurrency contract](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/concurrency.md).

## Database and network limits

Database APIs use `module.client(config)?` followed by receiver methods, with a bounded pool inside the client. Real server validation is separate for each driver, system, and commit. A fake ABI or successful link does not substitute for a real query.

Redis uses plaintext TCP. MySQL lacks consistent fail-closed certificate verification across Oracle/MariaDB. Direct connections should therefore use loopback, controlled private networks, or a verified TLS tunnel. Remote PostgreSQL requires explicit `sslmode=verify-full` and trusted root certificates. Native TLS in `std.net` does not mean HTTP, Redis, or the interpreter reuse that path.

## Self hosting and compatibility

Ku's self hosted lexer and parser have bounded subsets and differential checks. A complete self hosted parser, checker, IR, and backend remain unfinished; Rust still supplies the bootstrap toolchain. Version 0.0.x does not promise source or generated C/FFI ABI compatibility.

Work logs and previous CI results establish only their stated scope and commit. Use [release notes](/en/v0.0.18/releases/) to separate published changes from development work. This website does not report all current branch CI, production workloads, or database acceptance as passed.

## v0.0.18 release scope and known limits

This release freezes already implemented and verified functionality. It remains an **experimental prerelease**, not a production-readiness claim. [The v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18) is the authority for actual publication, assets and checksums; a skipped publishing job is not release evidence.

- JSON keeps the previous implementation as its default. `json.encode` / `json.decode` share the `stringify` / `parse` entry points. The float round-trip correction for `1e20` is included; yyjson replacement and performance exploration have ended.
- Task/native async support remains the documented bounded subset. Shutdown admission and cleanup fixes do not imply a complete M:N runtime or universal network suspension.
- HTTP uses bounded socket readiness, queues and response handling, with keep-alive support and optional bounded diagnostic records. These changes do not imply that all workload acceptance has passed.
- `ku supervise` controls whole-program workers, admission, queue bytes, output and cleanup. It is not one worker per HTTP request or per Task, and it is not automatically applied to ordinary `ku run`.

**Known HTTP issue:** HTTP 504 occurred historically, including a seven-request cluster. This round's 60-second and 180-second short tests did not reproduce it. The root cause is unconfirmed, and formal 30-minute stability acceptance has not passed. It is retained as a known issue for v0.0.18 and is not a separate release blocker. The earliest three historical 504s lack enough evidence for a unique reconstruction. Do not describe the issue as repaired or long-term stability as accepted.

**Deferred, not completed:** complete M:N, general Task network scheduling, the complete database platform matrix, and further long-running workload acceptance belong to later versions. General value copy/release follow-up and AST analysis remain paused. No unrelated memory-limit validation is being repeated.

### Process memory boundary

| Host | `ku supervise` guarantee and prerequisite |
| --- | --- |
| Linux | cgroup v2 kernel charged memory; a writable, delegated domain root with `memory` and `pids` enabled is required. Required interfaces include `memory.max`, `memory.swap.max`, `memory.oom.group`, `pids.max` and `cgroup.kill`. Missing controls or permissions cause refusal before application execution. |
| Windows | Nested Job Object limits for individual workers and the worker group, measured as committed virtual memory, not RSS. Initial process commit is checked before its suspended application thread is released. Host policy can reject Job assignment. |
| macOS | This isolation mode is not supported natively and is explicitly rejected before starting the application. Use a suitably configured Linux host/VM when required; there is no automatic VM bridge. |

Worker accounting covers applicable Ku and third-party library allocations in that worker's resource domain. Linux cgroup accounting and Windows commit are different metrics. Linux `memory.max` may permit transient overshoot; these mechanisms do not promise an absolute ceiling on all physical memory at every instant.

The coordinator itself has no OS memory hard limit. Its bounded admission/control reserve is an accounting policy, not a coordinator RSS limit. A single Task's entire heap is not precisely metered; shared connection pools, caches and library allocations cannot be exactly attributed to each Task. External databases, the host/VM and build compilers lie outside the worker domain.

When an OS limit terminates a worker, the coordinator reports failure and only returns reserved capacity after confirmed cleanup. Some allocation failures can be reported before allocation, but third-party exhaustion is not guaranteed to become an ordinary Result. **Database operations are never automatically retried**; an exited worker does not prove rollback, and side effects may be unknown.

See the [process execution contract](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/process-execution.md), [concurrency contract](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/concurrency.md) and [HTTP acceptance record](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/http-stability-acceptance.md). Cross-platform CI and installation checks establish only their reported targets and scope, not a completed real-database matrix.

## Run a controlled program

After preparing dependencies and an ordinary `hello.ku`, Linux requires a real delegated root supplied by the administrator or service manager:

```sh
ku supervise --worker-memory-bytes 134217728 --total-memory-bytes 536870912 \
  --max-workers 2 --max-queued-jobs 32 --max-queued-bytes 262144 \
  --max-output-bytes 1048576 --deadline-ms 30000 \
  --cgroup-root "$KU_CGROUP_ROOT" -- ./hello.ku
```

On Windows, use the same numeric limits and omit `--cgroup-root`. Decimal byte and millisecond values are required. `--` separates complete-program inputs; this is not a request-routing or general network scheduler API. The command emits configuration, per-execution and summary JSONL records and returns nonzero if a program fails. Native programs are built first and submitted with `--native`; compiler memory is outside the worker limit.
