---
title: 'Command-line tools'
description: 'Ku commands, complete shorthand reference, option scope and Runner requirements.'
kuVersion: '0.0.19'
verifiedAt: '2026-10-29'
sources: ['src/cli.rs', 'docs/package.md']
---

Use `ku -v` to print the version and `ku --help` for the installed CLI's top-level help.

## Everyday commands

```sh
ku -v
ku create hello
cd hello
ku check
ku run
```

A valid `ku.mod` selects a project's entry. Omitting the path does not guess `hello.ku` in an arbitrary directory. Run a single file directly with:

```sh
ku ./hello.ku
```

| Command                                         | Purpose                                                           |
| ----------------------------------------------- | ----------------------------------------------------------------- |
| `ku check main.ku`                              | Check without executing                                           |
| `ku check --json main.ku`                       | Emit JSON Lines diagnostics; silent on success                    |
| `ku run main.ku`                                | Interpret a program                                               |
| `ku build main.ku`                              | Build using Debug / Runner defaults                               |
| `ku build --backend c --release main.ku -o app` | Compile and link a native C program                               |
| `ku ir main.ku`                                 | Print optimized Ku IR                                             |
| `ku llvm empty.ku`                              | Emit experimental LLVM text IR, not an executable                 |
| `ku template list`                              | List basic, cli, http, json, fs and lib templates                 |
| `ku package resolve --locked`                   | Resolve using the project lock                                    |
| `ku package pack`                               | Create a source/dependency distribution, not a program executable |
| `ku package gc`                                 | Remove unused package cache entries                               |

## Full forms and shorthands

Aliases apply only in the relevant command's option positions.

| Function               | Full form                         | Shorthand or equivalent                     |
| ---------------------- | --------------------------------- | ------------------------------------------- |
| Version                | `ku --version`                    | `ku -v`, `ku -V`, `ku version`              |
| Top-level help         | `ku --help`                       | `ku -h`, `ku -help`, `ku help`              |
| Run one source file    | `ku run ./hello.ku`               | `ku ./hello.ku`                             |
| Create from a template | `ku create hello --template http` | `ku create hello -t http`                   |
| Initialize a template  | `ku init --template cli`          | `ku init -t cli`                            |
| Template list          | `ku template list`                | `ku create --list`                          |
| Output path            | `ku build --output PATH FILE`     | `ku build -o PATH FILE`                     |
| Release                | `--profile release`               | `--release`                                 |
| Debug                  | `--profile debug`                 | `--debug`                                   |
| Small                  | `--profile small`                 | `--small`                                   |
| Fast                   | `--profile fast`                  | `--fast`                                    |
| Additional LLVM text   | `--emit-llvm`                     | `--emit-ll`                                 |
| Verbose build log      | `ku build --verbose FILE`         | `ku build -v FILE`                          |
| Runner backend         | `--backend runner`                | `--backend interp`, `--backend interpreter` |
| C backend              | `--backend c`                     | `--backend native-c`                        |
| LLVM backend           | `--backend llvm`                  | `--backend ll`                              |

### The scope of `-v`

`ku -v` prints the Ku version. `ku build -v ./hello.ku` prints build details; it is not a version query. Profile shorthands and backend aliases belong to `build`, not to global options.

### Project entry and paths

`ku run`, `ku check`, `ku build` and `ku build .` use a valid project's configured entry. Applicable `ku package` subcommands use the current project context when their path is omitted. `ku build --target host` makes the same host-target selection as omitting that option.

Bare `ku` does not run a project; `ku run .` is not a project-directory shorthand. Implicit `ku ./hello.ku` accepts exactly that one path: no program arguments, `--offline` or `--locked`. Use `ku run --offline ./hello.ku` or `ku run --locked ./hello.ku` for dependency mode; do not combine the two modes.

Prefix special filenames with a relative path, such as `ku ./-hello.ku`, or `ku .\-hello.ku` on Windows. Quote paths containing spaces or Chinese characters. There are no `ku r`, `ku b`, `ku c`, `ku --v` aliases or program-argv forwarding.

## Which artifact is built?

| Invocation                                       | Artifact and requirements                                                                                                             |
| ------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------- |
| `ku build hello.ku`                              | Default Debug / Runner: a Rust wrapper packages interpreter execution; requires Rust matching the bundled runtime and a system linker |
| `ku build --backend c --release hello.ku -o app` | A native C executable; requires a supported C compiler/linker and native libraries used by the program                                |
| `ku build --emit-c hello.ku`                     | Still Runner by default, with additional C output; neither a C-only build nor a workaround for mismatched Runner toolchains           |
| `ku build --backend llvm empty.ku`               | Currently LLVM text IR, without a complete executable-linking path                                                                    |
| `ku package pack`                                | A source distribution, distinct from the program artifacts above                                                                      |

`check`, `run` and implicit execution need no Rust/C compiler. Runner embeds the entry file only; local imports still need their original module sources at runtime. Native C expands the import graph into a program independent of Ku source. Default output is under `.ku/build/<profile>/`; use the actual path printed by the CLI. Give Windows executables an `.exe` suffix.

The LLVM examples use `empty.ku` containing `fn main() {}`. This prototype supports a subset of syntax and built-ins; `--backend llvm` writes text IR and returns nonzero because executable linking is not supported.

### Historical `--native`

Existing calls are preserved. Prefer explicit `--backend c` in new commands:

```sh
ku build --native ./hello.ku
ku build --native ./hello.ku -o ./hello
```

The first emits C only. The second selects the compatibility branch that compiles and links a C program. `--native` must occupy its compatibility position immediately after `build`; it is not an unconditional alias accepted anywhere.

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
