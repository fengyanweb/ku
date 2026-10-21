---
title: 'Command-line tools'
description: 'Ku checking, execution, building, project and package commands.'
kuVersion: '0.0.18'
verifiedAt: '2026-10-21'
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
