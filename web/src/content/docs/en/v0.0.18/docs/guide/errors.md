---
title: Errors and troubleshooting
description: Diagnose Ku installation, imports, ownership, JSON, and backend capability errors.
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/diagnostics.md',
    'docs/syntax.md',
    'src/error.rs',
    'src/stdlib/json.rs',
  ]
sidebar:
  order: 2
slug: en/v0.0.18/docs/guide/errors
---

Compiler diagnostics can change during experimental development. Include the CLI version, Git commit, backend, full command, and minimal source when reporting a problem.

## Identify the Ku executable

```sh
ku --version
ku -h
ku check hello.ku
```

If `ku` is not found, invoke the executable from its extracted directory. Use `ku.exe` on Windows. If the default build is missing Rust or linking libraries, check the [installation requirements](/en/v0.0.18/docs/install/) and retain the complete archive layout, rather than copying only the CLI.

## Read diagnostics

Normal checking reports the file, source location, source excerpt, and available repair guidance. Tool integrations can use:

```sh
ku check --json hello.ku
```

The output is **JSON Lines**, with one diagnostic per line. Fields include `level`, `code`, `message`, `file`, `line`, `column`, `endLine`, `endColumn`, `notes`, and `helps`. Success is silent; an absence of JSON output does not mean the check did not run.

| Diagnostic | Common cause                        | What to do                                                                    |
| ---------- | ----------------------------------- | ----------------------------------------------------------------------------- |
| `E0105`    | Using `let`                         | Declare the variable through its first assignment                             |
| `E0302`    | A non-bool `if` / `while` condition | Write an explicit boolean comparison                                          |
| `E0601`    | Unknown standard library module     | Check the actual module path, such as `std.json`                              |
| `E0605`    | Missing required explicit import    | Import the module, for example `import fs from "std.fs"`                      |
| `E0901`    | Reading a moved value               | Explicitly clone an independent value, or use supported synchronous borrowing |
| `E0913`    | Borrowed async parameter            | Use an owned parameter; clone at the caller if the original is needed         |
| `E0918`    | Writing `&value` at the call site   | Pass an ordinary argument; the signature declares borrowing                   |

This summarizes registered diagnostics, rather than listing every error. The current authoritative registry is [src/error.rs](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/src/error.rs).

## Runtime errors use a separate contract

Runtime Error `domain/code` values are separate from compiler `E` codes. Invalid JSON can return `json/parse_error`; a missing object key can return `object/missing_key`. Use `?` after a fallible call, or read its fields in `try/catch`.

```ku
import json from "std.json"

fn main() {
    try {
        json.decode("[1,]")?
    } catch (error) {
        println(error.domain)
        println(error.code)
        println(error.message)
    }
}
```

This program intentionally triggers and catches a recoverable parse error, then exits normally. The example uses `ku run`; the same input returns `json/parse_error` across backends that support JSON, but `message` may differ. The interpreter includes a `json parse failed: ` prefix that native C may omit. Handle errors by `domain/code` rather than matching the complete message. A reported `byte N` is a zero based UTF-8 byte offset. It is not a character count around Chinese text or emoji.

## A native build is rejected

Passing `ku check` only establishes that frontend checking succeeded. Native C and LLVM each have capability limits. Check the [support matrix](/en/v0.0.18/docs/support/) when a construct is explicitly rejected. Increasing a timeout or succeeding with another backend does not verify the unsupported path.

`ku build --native file.ku` without `-o` only writes C. To compile and link an executable, use `ku build --backend c file.ku` and install the target C toolchain.

Database errors `execution_unknown` and `execution_completed_without_result` must not trigger automatic replay. Retain request and business evidence and establish the actual execution state first.

For more detail, see the [repository diagnostic guide](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/diagnostics.md).
