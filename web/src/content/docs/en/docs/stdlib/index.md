---
title: 'Standard library'
description: 'Find official Ku standard library modules and examples.'
kuVersion: '0.0.19'
verifiedAt: '2026-10-23'
sources: ['src/stdlib/metadata.rs', 'src/stdlib/mod.rs', 'docs/syntax.md']
sidebar:
  order: 0
---

The standard library ships with Ku. Import modules through `std`; no separate package installation is required. Each reference describes interfaces, return values, errors and examples.

## Data and collections

| Module            | Purpose                                          |
| ----------------- | ------------------------------------------------ |
| [string](string/) | Unicode strings, search, slicing and replacement |
| [array](array/)   | Array access, composition and mapping            |
| [object](object/) | String-keyed objects, fields and defaults        |
| [json](json/)     | JSON encoding, decoding and dynamic values       |

## Files, time and networking

| Module            | Purpose                                              |
| ----------------- | ---------------------------------------------------- |
| [fs](fs/)         | UTF-8 file I/O and directory listings                |
| [time](time/)     | Timestamps, dates, formatting, durations and waiting |
| [config](config/) | `.env` and YAML configuration                        |
| [http](http/)     | HTTP clients, routes, requests and responses         |
| [bytes](bytes/)   | Binary buffers                                       |
| [net](net/)       | TCP and TLS clients                                  |

## Databases and developer tools

| Module                          | Purpose                                      |
| ------------------------------- | -------------------------------------------- |
| [pg / redis / mysql](database/) | Database clients, pools and detached results |
| [lexer / parser](compiler/)     | Lexer and parser debugging summaries         |

## Imports

```ku
import json from "std.json"
import { http, time } from "std"
```

Examples import their dependencies explicitly. Databases, `bytes` and `net` currently require native C. See each module and the [support matrix](/en/docs/support/) for backend availability.

A return type `T!` denotes Result. After a call, `?` extracts success or propagates the error; inside `try`, that error can be handled by `catch`. Calls returning ordinary values such as `int` or `str` do not use `?`. See [error handling](/en/docs/guide/errors/#result-operators). An `&` parameter borrows read-only without consuming or implicitly copying its input. Allocating a new output string or array is separate from copying an input.

Browse the task-oriented [examples](/en/docs/examples/) or learn to write your own library in [packages and modules](/en/docs/guide/packages/). `std.task` is an internal observation interface; applications use `async fn` and `await`, described in the [concurrency guide](/en/docs/guide/concurrency/).
