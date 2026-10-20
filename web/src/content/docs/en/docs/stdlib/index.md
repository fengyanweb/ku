---
title: Standard library
description: Find Ku standard library modules, core signatures, and backend limits.
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'src/stdlib/metadata.rs',
    'src/stdlib/mod.rs',
    'docs/syntax.md',
    'docs/protocol-foundation.md',
  ]
---

This documentation describes the frozen scope of the v0.0.18 experimental prerelease. Actual publication and downloadable assets are recorded in GitHub Releases. These core entry points were checked against source; this is not an exhaustive API reference. The [syntax contract](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/syntax.md) defines behavior. See the [support matrix](/en/docs/support/) for backend limits.

## Import modules

```ku
import fs from "std.fs"
import json from "std.json"
import { http, time } from "std"
```

`fs`, `http`, `config`, `task`, `pg`, `redis`, `mysql`, `bytes`, and `net` require explicit imports. Some historical modules can still be called directly; new examples prefer explicit imports to make dependencies clear. `std.task` is an internal observation and stress testing namespace, rather than an application concurrency API.

| Module                               | Purpose                                      | Limits to remember                                                                                   |
| ------------------------------------ | -------------------------------------------- | ---------------------------------------------------------------------------------------------------- |
| `std.string` / string methods        | Unicode length, byte length, lookup, slicing | Native `trim/lower/upper` lack a Unicode implementation                                              |
| `std.array` / array methods          | Length, lookup, new arrays, map              | Direct index failure differs from recoverable `try_get`                                              |
| `std.object`                         | Explicit fallback lookup with `get_or`       | A missing ordinary index does not default to null                                                    |
| [std.json](/en/docs/stdlib/json/)    | Strict JSON encoding and decoding            | Unsupported in native Task and LLVM                                                                  |
| `std.fs`                             | Files, existence checks, directory listing   | Relative paths resolve from the current `.ku` source directory                                       |
| `std.time`                           | Time, dates, durations, sleep                | Synchronous and Task waits use different execution paths                                             |
| `std.http`                           | HTTP clients, services, responses            | Backend and lifecycle differences remain; this is not a complete generic Task network implementation |
| `std.config`                         | Environment and configuration                | Do not commit real credentials in source                                                             |
| `std.pg` / `std.redis` / `std.mysql` | Unified clients with bounded pools           | Native only; real server validation and TLS limits need separate checks                              |
| `std.bytes` / `std.net`              | Binary data, TCP client, optional TLS        | Generated native C path; TLS needs a matching target pack                                            |
| `std.lexer` / `std.parser`           | Compiler debugging helpers                   | Return debug summaries, not complete public Token / AST structures                                   |

## Common signatures

These are documentation signatures. `&` denotes a supported read only parameter mode; `T!` denotes a recoverable result:

```text
len(&value:any): int
str(&value:any): str
println(&value:any): null
string.len(&text:str): int
string.byte_len(&text:str): int
string.slice(text:str, start:int, end:int): str!
array.len(&values:[T]): int
array.try_get(values:[T], index:int): T!
fs.read(path:str): str!
fs.write(path:str, text:str): null!
fs.exists(path:str): bool
fs.read_dir(path:str): [str]!
```

`"中😀".len()` is 2 and `.byte_len()` is 7. Length counts Unicode scalar values; slicing uses character indices rather than UTF-8 bytes. `array.push` and `array.concat` return new arrays without mutating their inputs.

## Read a file

```ku
import fs from "std.fs"

fn main(): null! {
    text = fs.read("message.txt")?
    println(text)
    return ok(null)
}
```

Create a UTF-8 `message.txt` beside the source first. A missing file or failed read returns an error. This example depends on that input file. `try_read` and `try_write` remain compatible aliases with the same result types.

`read_dir` returns sorted, independently owned paths, limited to 10,000 entries and 1,000,000 total path bytes. Restricted native Task file operations use a fixed background pool. This does not establish generic network waiting through the same mechanism.

## Maintaining the reference

Public interface metadata lives in [src/stdlib/metadata.rs](https://github.com/fengyanweb/ku/blob/v0.0.18/src/stdlib/metadata.rs). It is checked alongside implementations and contracts. This website currently maintains a curated manual reference; no existing `ku doc` command has been confirmed.
