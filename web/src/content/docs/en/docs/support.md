---
title: Platform and backend support
description: Distinguish Ku release platforms, interpreter, runner, native C, Task subset, and LLVM support.
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-28'
sources:
  [
    'docs/syntax.md',
    'docs/v0.0.19.md',
    'docs/v0.0.18-worklog.md',
    'docs/protocol-foundation.md',
    '.github/workflows/native-three-os.yml',
    'src/service_supervision.rs',
    'src/ir/task_lower.rs',
    'src/backend/c_http_runtime.c',
    'src/backend/c_task_pg_runtime.c',
  ]
---

<span id="execution"></span>

## Choose a platform and execution path

Use [installation](/en/docs/install/) and the actual [release assets](https://github.com/fengyanweb/ku/releases) to select a CLI for your system and architecture. A source build, CI job or isolated deployment rehearsal does not by itself publish a downloadable platform package. The Linux ARM64 release-candidate rehearsal covers its sample service and dependencies; a real application, its production SLO and canary acceptance are separate.

| Capability                 | Interpreter `ku run` / default runner                                     | Native C                                                  | Native Task path                                                                  |
| -------------------------- | ------------------------------------------------------------------------- | --------------------------------------------------------- | --------------------------------------------------------------------------------- |
| Language and local imports | Interpreter; runner embeds the entry but still needs local import sources | Supported source graph is expanded during generation      | Restricted typed subset                                                           |
| JSON                       | General encode/decode                                                     | General synchronous encode/decode                         | Encode a scalar object literal only; no general decode                            |
| HTTP                       | Client and synchronous handlers                                           | Synchronous server subset, including `http.json`          | Named async handlers with supported request/response types                        |
| PostgreSQL / Redis / MySQL | No database client implementation                                         | Pooled native clients with target-compatible dependencies | PG query suspension in a supported async HTTP handler; not generic database async |
| Time                       | Calendar, durations and waits                                             | Selected clocks                                           | Integer-millisecond sleep                                                         |
| TCP, TLS and file I/O      | See the individual module                                                 | Selected native interfaces                                | Only explicitly lowered operations; not arbitrary network suspension              |

See [HTTP](/en/docs/stdlib/http/), [JSON](/en/docs/stdlib/json/), [time](/en/docs/stdlib/time/) and [database clients](/en/docs/stdlib/database/) for parameters, errors and complete examples. Type checking is separate from backend compilation. Default runner builds need a matching Rust toolchain and archive layout; native builds need a target C compiler and relevant native libraries. The LLVM text backend remains a limited prototype.

## Task support

An async call submits work and returns a one-use Task handle. `await` obtains its result; it does not define when execution first starts. The native Task path supports its listed primitive/string/flat-array operations, Results, Task start/move/await, timers and selected bounded I/O. It also supports named async HTTP handlers and the narrowly documented PG capture path. This is not full M:N execution, every async closure/capture combination, general JSON manipulation or a universal network scheduler.

Owned inputs, cancellation, shared cleanup deadlines and supported operations still apply. A successful arithmetic Task does not establish support for a whole library. Follow the [Task guide](/en/docs/guide/concurrency/) and the [concurrency contract](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/concurrency.md).

## Application budgets and process limits

These controls cover different resources:

| Control                          | Scope and limit                                                                                                                                |
| -------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- |
| HTTP service options             | Connections, active/pending requests and controlled transport buffers for that service; not its entire heap                                    |
| Task and shared database budgets | Runtime-managed slots, pending operations and tracked allocations; shared pools and foreign allocations are not exact per-Task heap accounting |
| `ku service`, ordinary mode      | Supervises one resident native HTTP instance, authenticated health/drain/metrics and bounded restart policy; no OS memory hard limit           |
| `ku supervise`                   | Runs bounded whole-program jobs; it is not an HTTP request scheduler                                                                           |
| Explicit OS memory mode          | Linux cgroup v2 charged memory or Windows Job committed memory, subject to the prerequisites below                                             |

`ku service` ordinary mode is available on Linux, macOS and Windows. Read `ku service --help` and the [CLI guide](/en/docs/tools/cli/) before configuring it. It does not attach limits to an unrelated `ku run`. There is no public `app.stop()` method: supervised native health/drain control and interpreter listener `close()` have different lifecycles. A fail-stop process exit, including native exit code 80 on relevant cleanup failures, is a service failure and needs a finite supervisory policy.

| Host    | Explicit OS memory mode                                                                                                                                                                                                                |
| ------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Linux   | Delegated cgroup v2 domain with writable memory/pids controls, including `memory.max`, `memory.swap.max`, `memory.oom.group`, `pids.max` and `cgroup.kill`. Missing permissions or controls cause refusal before the application runs. |
| Windows | Job Object limits use committed virtual memory, not RSS. Initial commit and Job assignment are checked before application execution; host policy can reject nesting.                                                                   |
| macOS   | Native hard memory mode is rejected. Ordinary service supervision is a separate supported mode; no automatic Linux VM bridge is provided.                                                                                              |

Linux charged memory, Windows commit and RSS are different measures. Linux limits can permit transient overshoot; none is a promise about all host physical memory at every instant. The coordinator, external PostgreSQL/Redis/MySQL servers, proxy, host/VM and build compilers are outside the managed worker's memory domain. Fixed control metadata also consumes real process memory.

After preparing a real delegated Linux root and a basic `hello.ku`, a bounded whole-program example is:

```sh
ku supervise --worker-memory-bytes 134217728 --total-memory-bytes 536870912 \
  --max-workers 2 --max-queued-jobs 32 --max-queued-bytes 262144 \
  --max-output-bytes 1048576 --deadline-ms 30000 \
  --cgroup-root "$KU_CGROUP_ROOT" -- ./hello.ku
```

On Windows omit `--cgroup-root`. All numeric values above are decimal bytes or milliseconds. `--` separates complete program inputs; native executables are built first and submitted with `--native`. Compilers are outside these limits. Failed workers release reservations only after confirmed cleanup. Allocation failure or a killed foreign library is not guaranteed to become an ordinary Result.

## Database connections and failure handling

Database clients are native-only. A real-server check applies to its driver, platform, dependency and source identity; successful linking is not a query test. PostgreSQL remote connections need explicit `sslmode=verify-full` and trusted certificates. Redis is plaintext TCP. MySQL does not provide a uniform fail-closed certificate/hostname verification contract across Oracle/MariaDB. Use a controlled local/private connection or independently verified tunnel where required. `std.net` TLS does not imply TLS for every library.

**Statements are never automatically retried.** Timeout, cancellation, worker exit and connection reset do not prove rollback. Preserve uncertain outcomes and reconcile writes using business identifiers before considering a manual retry.

## Version compatibility

Ku remains under development. Complete self-hosted checking/IR/backend, generic Task network scheduling and a complete database/platform matrix are not promised. Rust supplies the bootstrap toolchain. Version 0.0.x does not guarantee source or generated C/FFI ABI compatibility.

## Known issues

As of 2026-10-28, the three-platform release gates and package verification for 0.0.19 have passed. These latency issues remain:

- **Intermittent HTTP 504s** occurred historically but were not reproduced in the latest short recheck. Their root cause is unconfirmed.
- **Occasional latency tails during cancellation** remain. In the latest 30-second recheck, all 60,000 short requests were correct and p99 was 1.557 ms; the maximum was 98.809 ms, with a small number above 20 ms.

These short measurements do not replace formal 30-minute stability acceptance, which has not passed. See the [0.0.19 known-issue record](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/v0.0.19-known-issues.en.md). Report a problem through [Issues](https://github.com/fengyanweb/ku/issues) or email [kagura.gc@qq.com](mailto:kagura.gc@qq.com).
