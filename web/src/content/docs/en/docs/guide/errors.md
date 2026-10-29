---
title: Errors and troubleshooting
description: Diagnose Ku installation, imports, ownership, JSON, and backend capability errors.
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-28'
sources:
  [
    'docs/diagnostics.md',
    'docs/syntax.md',
    'src/error.rs',
    'src/stdlib/json.rs',
  ]
sidebar:
  order: 2
---

Compiler diagnostics can change during experimental development. Include the CLI version, Git commit, backend, full command, and minimal source when reporting a problem.

<a id="result-operators"></a>

## When to write `!` and `?`

The meaning depends on where the symbol appears:

| Form                            | Meaning                                                                                                                       | Example                |
| ------------------------------- | ----------------------------------------------------------------------------------------------------------------------------- | ---------------------- |
| `!` after a type                | `T!` denotes Result: success contains a `T`, and failure contains an Error.                                                   | `fn read_name(): str!` |
| `?` after an expression         | Extract success; on failure, transfer control to an enclosing `try/catch` or return the error from the current `T!` function. | `name = read_name()?`  |
| `!` before a boolean expression | Negate a `bool`; this is unrelated to Result.                                                                                 | `!ready`               |

Not every function needs a `!` return type, and not every call needs `?`. Calls such as `println(...)`, `text.len()`, and direct array indexing return ordinary values and do not use `?`. The `?` operator does not denote an optional value or turn an error into `null`.

### A plain entry point

For ordinary calculations, or when errors are handled inside the function with `try/catch`, write `fn main()` without a return type annotation. The `main` function itself is still the required entry point.

```ku
fn main() {
    ready = false
    println(!ready)
}
```

This program prints `true`. It needs neither `: null!` nor `return ok(null)`.

### Returning an error

Declare `T!` when a function should return recoverable errors to its caller. Return a successful value with `ok(value)`:

```ku
fn read_name(): str! {
    return ok("Ku")
}

fn main(): null! {
    println(read_name()?)
    return ok(null)
}
```

Here `read_name()` returns Result, and `?` extracts its string. If it returns an error, later statements do not run: the error leaves `main`, and the CLI reports it and exits with a nonzero status. `null!` means that success carries no application value, while failure can still carry an Error.

Removing only `: null!` while keeping the outer `?` in this example is invalid. To handle the error within a plain `main`, put the call in `try/catch`, as in the JSON example below. A `try` block still needs `?` to check a Result; placing a Result-returning call inside it does not automatically unwrap that Result or trigger `catch`.

## Identify the Ku executable

```sh
ku --version
ku -h
ku check hello.ku
```

If `ku` is not found, invoke the executable from its extracted directory. Use `ku.exe` on Windows. If the default build is missing Rust or linking libraries, check the [installation requirements](/en/docs/install/) and retain the complete archive layout, rather than copying only the CLI.

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

This summarizes registered diagnostics, rather than listing every error. The current authoritative registry is [src/error.rs](https://github.com/fengyanweb/ku/blob/v0.0.19/src/error.rs).

## Runtime errors use a separate contract

Runtime Error `domain/code` values are separate from compiler `E` codes. Invalid JSON can return `json/parse_error`; a missing object key can return `object/missing_key`. Use `?` after a fallible call, or read its fields in `try/catch`.

```ku
import json from "std.json"

fn main() {
    try {
        json.parse("[1,]")?
    } catch (error) {
        println(error.domain)
        println(error.code)
        println(error.message)
    }
}
```

This teaching program intentionally triggers a recoverable parse failure. A reported `byte N` is a zero based UTF-8 byte offset. It is not a character count around Chinese text or emoji.

## A native build is rejected

Passing `ku check` only establishes that frontend checking succeeded. Native C and LLVM each have capability limits. Check the [support matrix](/en/docs/support/) when a construct is explicitly rejected. Increasing a timeout or succeeding with another backend does not verify the unsupported path.

`ku build --native file.ku` without `-o` only writes C. To compile and link an executable, use `ku build --backend c file.ku` and install the target C toolchain.

Database errors `execution_unknown` and `execution_completed_without_result` must not trigger automatic replay. Retain request and business evidence and establish the actual execution state first.

For more detail, see the [repository diagnostic guide](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/diagnostics.md).
