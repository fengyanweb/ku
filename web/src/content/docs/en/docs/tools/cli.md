---
title: 'Command-line tools'
description: 'Ku checking, execution, building, project and package commands.'
kuVersion: '0.0.19'
verifiedAt: '2026-10-23'
sources: ['src/cli.rs', 'docs/package.md', 'docs/process-execution.md']
---

Use `ku --version` to identify the executable and `ku --help` to inspect available options.

| Command                               | Purpose                                            |
| ------------------------------------- | -------------------------------------------------- |
| `ku check main.ku`                    | Check without executing                            |
| `ku check --json main.ku`             | Emit JSON Lines diagnostics for tools              |
| `ku run main.ku`                      | Interpret a program                                |
| `ku build main.ku`                    | Build the default runner                           |
| `ku build --backend c main.ku -o app` | Compile and link a native C executable             |
| `ku build --native main.ku`           | Emit C only, without linking                       |
| `ku ir main.ku`                       | Print optimized IR                                 |
| `ku llvm main.ku`                     | Emit experimental LLVM text, without an executable |
| `ku create Hello --template basic`    | Create a project                                   |
| `ku init --template basic`            | Initialize the current directory                   |
| `ku template list`                    | List templates                                     |
| `ku package resolve --locked`         | Resolve using the project lock                     |
| `ku package gc`                       | Remove unused package cache entries                |

## Choose an execution path

`check` and `run` do not require Rust or C compilers. Default `build` needs Rust 1.89.0, a linker and the archive's full `libku.rlib` / `deps` layout. Its runner embeds only the entry file; local imports still need their original sources. Native C needs a C compiler, expands the import graph and produces a program independent of Ku source files.

Use `app.exe` on Windows. See [installation](/en/docs/install/) and [backend support](/en/docs/support/). There is no `ku fmt`, `ku test` or `ku doc` command; VS Code formatting is an extension feature.

## Controlled execution

`ku supervise` explicitly manages whole-program workers. It is not automatically applied to `run` or individual HTTP requests. Linux requires delegated cgroup v2; Windows limits committed memory. This isolation mode is not supported natively on macOS. See [platform and process limits](/en/docs/support/) for setup and failure behavior.

## Resident native HTTP services

`ku service --help` describes the separate resident-service supervisor. Compile the [native HTTP example](/en/docs/stdlib/http/) first. The executable must support the native authenticated health/drain protocol; the probe and control addresses must be distinct nonzero loopback addresses with free ports.

Before this example, create a private regular `service.token` file containing 32–128 ASCII letters/digits; on Unix it must be accessible only to its owner (`chmod 600 service.token`). Supply your own unpredictable token, not a value committed with source. Then run:

```sh
ku service --probe-addr 127.0.0.1:18181 \
  --control-addr 127.0.0.1:19081 --control-token-file service.token \
  --startup-timeout-ms 5000 --probe-timeout-ms 250 \
  --probe-interval-ms 1000 --failure-threshold 3 \
  --drain-timeout-ms 10000 --restart-limit 5 --restart-window-ms 60000 \
  --backoff-ms 100 --max-backoff-ms 5000 -- ./native-server
```

These values are the supervisor's defaults made explicit. Numeric durations are milliseconds; zero is invalid. `--failure-threshold` is 1–100 and `--restart-limit` is 1–1000. Backoff must not exceed max-backoff, and probe-timeout must not exceed startup-timeout. The executable and arguments follow `--`; Windows uses its actual `.exe` path.

The control listener exposes `GET /status`, `GET /metrics` and `POST /stop` with `X-Ku-Control-Token` authentication. It manages health, admission/drain and bounded restart of one service instance; it is not a request proxy or database retry mechanism. Managed-program stdout/stderr go to the null device in this mode; use its control metrics rather than assuming application logs are retained.

Without memory options this ordinary mode does not enforce an OS memory ceiling. Explicit memory mode requires **both** `--worker-memory-bytes` and `--total-memory-bytes`; Linux additionally needs a delegated `--cgroup-root`. Read [platform limits](/en/docs/support/) first. Native macOS rejects this hard-limit mode while allowing ordinary supervision. Metrics distinguish current state and failures; successful health does not prove a business SLO or stable production backend.
